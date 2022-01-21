use bns_node::discoveries::http::remote_handler;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bns_core::transports::default::DefaultTransport;
use bns_core::types::ice_transport::IceTransport;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

#[tokio::main]
async fn main() -> Result<()> {
    let http_addr = "0.0.0.0:60000";
    let remote_addr = "0.0.0.0:50000";

    let ice_transport = DefaultTransport::new().await?;
    let peer_connection = Arc::downgrade(&ice_transport.get_peer_connection().await.unwrap());
    let pending_candidates = ice_transport.get_pending_candidates().await;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    ice_transport
        .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let peer_connection = peer_connection.to_owned();
            let pending_candidates = pending_candidates.to_owned();
            Box::pin(async move {
                if let Some(candidate) = c {
                    if let Some(peer_connection) = peer_connection.upgrade() {
                        let desc = peer_connection.remote_description().await;
                        if desc.is_none() {
                            let mut candidates = pending_candidates;
                            println!("start answer candidate: {:?}", candidate);
                            candidates.push(candidate.clone());
                        }
                    }
                }
            })
        }))
        .await?;
    ice_transport
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            // Failed to exit dial server
            if s == RTCPeerConnectionState::Failed {
                let _ = done_tx.try_send(());
            }

            Box::pin(async {})
        }))
        .await?;
    ice_transport
        .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let d_label = d.label().to_owned();
            let d_id = d.id();
            println!("New DataChannel {} {}", d_label, d_id);
            Box::pin(async move{
                // Register channel opening handling
                let d2 =  Arc::clone(&d);
                let d_label2 = d_label.clone();
                let d_id2 = d_id;
                d.on_open(Box::new(move || {
                    println!("Data channel '{}'-'{}' open. ", d_label2, d_id2);
                    print!("Random messages will now be sent to any connected DataChannels every 5 seconds");
                    Box::pin(async move {
                        let mut result = Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(5));
                            tokio::pin!(timeout);

                            tokio::select! {
                                _ = timeout.as_mut() =>{
                                    let message = math_rand_alpha(15);
                                    println!("Sending '{}'", message);
                                    result = d2.send_text(message).await.map_err(Into::into);
                                }
                            };
                        }
                    })
                })).await;

                // Register text message handling
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                    println!("Message from DataChannel '{}': '{}'", d_label, msg_str);
                    Box::pin(async{})
                })).await;
            })
        }))
        .await?;

    tokio::spawn(async move {
        let ice_transport = ice_transport.clone();
        let service = make_service_fn(move |_| {
            let ice_transport = ice_transport.to_owned();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    remote_handler(req, remote_addr.to_string(), ice_transport.to_owned())
                }))
            }
        });

        let http_addr = SocketAddr::from_str(&http_addr).unwrap();
        let server = Server::bind(&http_addr).serve(service);
        // Run this server for... forever!
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });

    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("");
        }
    };
    Ok(())
}
