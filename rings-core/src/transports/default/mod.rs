/// A default transport use for node.
pub mod transport;

pub use transport::DefaultTransport;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

use crate::types::ice_transport::IceCandidate;

impl From<RTCIceCandidateInit> for IceCandidate {
    fn from(cand: RTCIceCandidateInit) -> Self {
        Self {
            candidate: cand.candidate.clone(),
            sdp_mid: cand.sdp_mid.clone(),
            sdp_m_line_index: cand.sdp_mline_index,
            username_fragment: cand.username_fragment,
        }
    }
}

impl From<IceCandidate> for RTCIceCandidateInit {
    fn from(cand: IceCandidate) -> Self {
        Self {
            candidate: cand.candidate.clone(),
            sdp_mid: cand.sdp_mid.clone(),
            sdp_mline_index: cand.sdp_m_line_index,
            username_fragment: cand.username_fragment,
        }
    }
}
