#[tokio::main]
async fn main() {
    let result = match setups_telegram_relay::config::Config::from_env() {
        Ok(config) => setups_telegram_relay::run(config).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("relay_lifecycle state=failed code={}", error.code());
        std::process::exit(1);
    }
}
