use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

mod config;
mod csrf;
mod handlers;
mod state;
mod storage;
mod templates;

use self::config::Config;
use self::state::State;

#[async_std::main]
async fn main() -> tide::Result<()> {
    // Setup logging & tracing
    let fmt_layer = tracing_subscriber::fmt::layer();
    let filter_layer = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?;

    let subscriber = Registry::default().with(filter_layer).with(fmt_layer);
    subscriber.try_init()?;

    // Loading the config
    let config = Config::load()?;
    let address = config.listener.address.clone();

    // Load and compile the templates
    let templates = self::templates::load()?;

    // Create the shared state
    let state = State::new(config, templates);

    // Start the server
    let mut app = tide::with_state(state);
    app.with(tide_tracing::TraceMiddleware::new());
    self::handlers::install(&mut app);
    app.listen(address).await?;
    Ok(())
}
