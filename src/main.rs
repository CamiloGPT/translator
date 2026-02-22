use anyhow::Result;
use axum::{routing::post, Router};
use base64::{engine::general_purpose::STANDARD, Engine};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::{SinkExt, StreamExt};
use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, Player};
use serde_json::json;
use std::num::{NonZeroU16, NonZeroU32};
use std::{env, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{interval, Duration},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    dotenvy::dotenv().ok();

    let app = Router::new().route("/start", post(start));

    println!("Running on http://localhost:8080");

    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn start() -> &'static str {
    tokio::spawn(async {
        if let Err(e) = run().await {
            eprintln!("Error: {:?}", e);
        }
    });

    "Translator started"
}

async fn run() -> Result<()> {
    let api_key = env::var("OPENAI_API_KEY")?;

    let url = "wss://api.openai.com/v1/realtime?model=gpt-4o-realtime-preview";

    let mut req = url.into_client_request()?;

    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", api_key).parse()?);

    req.headers_mut()
        .insert("OpenAI-Beta", "realtime=v1".parse()?);

    let (ws, _) = connect_async(req).await?;
    let (mut write, mut read) = ws.split();

    // Configure session
    write
        .send(Message::Text(
            json!({
                "type": "session.update",
                "session": {
                    "modalities": ["audio"],
                    "instructions": "Translate everything from Spanish to natural English speech in real time.",
                    "voice": "alloy",
                    "input_audio_format": "pcm16",
                    "output_audio_format": "pcm16"
                }
            })
            .to_string()
            .into(),
        ))
        .await?;

    println!("Realtime session configured");

    let (tx, mut rx) = mpsc::channel::<Vec<i16>>(32);

    spawn_mic_capture(tx)?;

    // Audio output setup (rodio 0.22.1)
    let handle = DeviceSinkBuilder::open_default_sink()?;
    let player = Player::connect_new(&handle.mixer());
    let player = Arc::new(player);
    let player_clone = player.clone();

    let mut commit_interval = interval(Duration::from_millis(200));

    // Writer task
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(samples) = rx.recv() => {
                    let bytes: &[u8] = bytemuck::cast_slice(&samples);
                    let b64 = STANDARD.encode(bytes);

                    let msg = json!({
                        "type": "input_audio_buffer.append",
                        "audio": b64
                    });

                    let _ = write.send(Message::Text(msg.to_string().into())).await;
                }
                _ = commit_interval.tick() => {
                    let commit_msg = json!({
                        "type": "input_audio_buffer.commit"
                    });

                    let _ = write.send(Message::Text(commit_msg.to_string().into())).await;
                }
            }
        }
    });

    // Reader task
    let reader = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            if let Ok(Message::Text(text)) = msg {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                    if parsed["type"] == "response.output_audio.delta" {
                        if let Some(b64) = parsed["delta"].as_str() {
                            if let Ok(bytes) = STANDARD.decode(b64) {
                                let samples_i16: Vec<i16> = bytes
                                    .chunks_exact(2)
                                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                                    .collect();

                                let samples_f32: Vec<f32> = samples_i16
                                    .into_iter()
                                    .map(|s| s as f32 / i16::MAX as f32)
                                    .collect();

                                let source = SamplesBuffer::new(
                                    NonZeroU16::new(1).unwrap(),
                                    NonZeroU32::new(24000).unwrap(),
                                    samples_f32,
                                );

                                player_clone.append(source);
                            }
                        }
                    }
                }
            }
        }
    });

    writer.await?;
    reader.await?;

    Ok(())
}

fn spawn_mic_capture(tx: mpsc::Sender<Vec<i16>>) -> Result<()> {
    std::thread::spawn(move || {
        let host = cpal::default_host();
        let device = host.default_input_device().expect("No input device");
        let config = device.default_input_config().expect("No default config");

        let stream = device
            .build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    let _ = tx.blocking_send(data.to_vec());
                },
                move |err| eprintln!("Mic error: {:?}", err),
                None,
            )
            .unwrap();

        stream.play().unwrap();

        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });

    Ok(())
}
