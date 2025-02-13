//! Logging configuration contains both `node` and `browser`.
use tracing::Level;
use tracing_log::LogTracer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

#[cfg(feature = "node")]
/// logging configuration about node.
pub mod node {
    use std::panic::PanicInfo;

    use backtrace::Backtrace;
    use clap::ValueEnum;
    use opentelemetry::global;
    use opentelemetry::sdk::propagation::TraceContextPropagator;
    use tracing::error;
    use tracing_subscriber::filter;
    use tracing_subscriber::fmt;
    use tracing_subscriber::Layer;

    use super::*;

    #[derive(ValueEnum, Debug, Clone)]
    pub enum LogLevel {
        Debug,
        Info,
        Warn,
        Error,
        Trace,
    }

    impl From<LogLevel> for Level {
        fn from(val: LogLevel) -> Self {
            match val {
                LogLevel::Trace => Level::TRACE,
                LogLevel::Debug => Level::DEBUG,
                LogLevel::Info => Level::INFO,
                LogLevel::Warn => Level::WARN,
                LogLevel::Error => Level::ERROR,
            }
        }
    }

    fn log_panic(panic: &PanicInfo) {
        let backtrace = Backtrace::new();
        let backtrace = format!("{:?}", backtrace);
        if let Some(location) = panic.location() {
            error!(
                message = %panic,
                backtrace = %backtrace,
                panic.file = location.file(),
                panic.line = location.line(),
                panic.column = location.column(),
            );
        } else {
            error!(message = %panic, backtrace = %backtrace);
        }
    }

    pub fn set_panic_hook() {
        // Set a panic hook that records the panic as a `tracing` event at the
        // `ERROR` verbosity level.
        //
        // If we are currently in a span when the panic occurred, the logged event
        // will include the current span, allowing the context in which the panic
        // occurred to be recorded.
        std::panic::set_hook(Box::new(|panic| {
            log_panic(panic);
        }));
    }

    pub fn init_logging(level: Level) {
        set_panic_hook();

        let subscriber = Registry::default();
        let level_filter = filter::LevelFilter::from_level(level);

        // Filter floating log of mdns
        let mdns_log_filter = filter::FilterFn::new(|metadata| {
            !metadata.target().starts_with("webrtc_mdns::conn")
                || [276, 322]
                    .iter()
                    .all(|&line| !metadata.line().unwrap_or_default() == line)
        });

        // Stderr
        let subscriber = subscriber.with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(level_filter)
                .with_filter(mdns_log_filter.clone()),
        );

        // Jaeger
        let subscriber = {
            if let Ok(endpoint) = std::env::var("RINGS_JAEGER_AGENT_ENDPOINT") {
                global::set_text_map_propagator(TraceContextPropagator::new());
                let jaeger = opentelemetry_jaeger::new_agent_pipeline()
                    .with_service_name("rings")
                    .with_endpoint(endpoint)
                    .with_auto_split_batch(true)
                    .install_batch(opentelemetry::runtime::Tokio)
                    .expect("opentelemetry_jaeger install");
                subscriber.with(Some(
                    tracing_opentelemetry::layer()
                        .with_tracer(jaeger)
                        .with_filter(level_filter)
                        .with_filter(mdns_log_filter),
                ))
            } else {
                subscriber.with(None)
            }
        };

        // Enable log compatible layer to convert log record to tracing span.
        // We will ignore any errors that returned by this functions.
        let _ = LogTracer::init();

        // Ignore errors returned by set_global_default.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

#[cfg(feature = "browser")]
pub mod browser {
    use tracing_wasm::WASMLayer;
    use tracing_wasm::WASMLayerConfigBuilder;

    use super::*;

    pub fn set_panic_hook() {
        // When the `console_error_panic_hook` feature is enabled, we can call the
        // `set_panic_hook` function at least once during initialization, and then
        // we will get better error messages if our code ever panics.
        //
        // For more details see
        // https://github.com/rustwasm/console_error_panic_hook#readme
        // This is not needed for tracing_wasm to work, but it is a common tool for getting proper error line numbers for panics.
        #[cfg(feature = "console_error_panic_hook")]
        console_error_panic_hook::set_once();
    }

    pub fn init_logging(level: Level) {
        set_panic_hook();

        let subscriber = Registry::default();

        // Browser console and profiler
        let subscriber = subscriber.with(WASMLayer::new(
            WASMLayerConfigBuilder::new().set_max_level(level).build(),
        ));

        //TODO: Jaeger in browser. How to setup agent endpoint?

        // Enable log compatible layer to convert log record to tracing span.
        // We will ignore any errors that returned by this functions.
        let _ = LogTracer::init();

        // Ignore errors returned by set_global_default.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}
