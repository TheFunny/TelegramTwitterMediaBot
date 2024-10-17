use crate::handlers::message_handler;
use dotenv::dotenv;
use std::env;
use std::net::IpAddr;
use teloxide::dptree::endpoint;
use teloxide::types::InputFile;
use teloxide::{prelude::*, update_listeners::webhooks};

mod handlers;

#[tokio::main]
async fn main() {
    dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting bot");

    let bot = Bot::from_env();

    let handler =
        dptree::entry().branch(Update::filter_message().branch(endpoint(message_handler)));

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![""])
        .enable_ctrlc_handler()
        .build();

    match env::var("WEBHOOK") {
        Ok(v) if v.parse::<bool>().unwrap_or(false) => {
            let url: url::Url = env::var("WEBHOOK_URL")
                .expect("WEBHOOK_URL is not set")
                .parse()
                .expect("Failed to parse WEBHOOK_URL");
            bot.set_webhook(url.clone()).await.unwrap();
            let mut options = webhooks::Options::new(
                (
                    env::var("WEBHOOK_LISTEN")
                        .expect("WEBHOOK_LISTEN is not set")
                        .parse::<IpAddr>()
                        .expect("Failed to parse WEBHOOK_LISTEN"),
                    env::var("WEBHOOK_PORT")
                        .expect("WEBHOOK_PORT is not set")
                        .parse()
                        .expect("Failed to parse WEBHOOK_PORT"),
                )
                    .into(),
                url,
            );
            if let Ok(cert) = env::var("WEBHOOK_CERT") {
                options = options.certificate(InputFile::file(cert));
            }
            if let Ok(secret) = env::var("WEBHOOK_SECRET_TOKEN") {
                options = options.secret_token(secret);
            }

            dispatcher
                .dispatch_with_listener(
                    webhooks::axum(bot.clone(), options)
                        .await
                        .expect("Failed to create webhook listener"),
                    LoggingErrorHandler::with_custom_text("Error from update listener"),
                )
                .await;
        }
        _ => {
            dispatcher.dispatch().await;
        }
    }
}
