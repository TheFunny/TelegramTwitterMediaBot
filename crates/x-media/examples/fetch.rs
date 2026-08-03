use x_media::site;

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: cargo run -p x-media --example fetch -- <url>");
    let result = site::fetch(&url).await;
    println!("{result:#?}");
}
