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
use tokio::{net::TcpListener, sync::mpsc};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};

#[tokio::main]
async fn main() {
    // Install default Rustls crypto provider for TLS support
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    dotenvy::dotenv().ok();

    let app = Router::new().route("/start", post(start));

    println!("Server running on http://localhost:8080");

    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn start() -> &'static str {
    tokio::spawn(async {
        if let Err(e) = run().await {
            eprintln!("Error during translation runtime: {:?}", e);
        }
    });

    println!("Translator started");
    "Translator started"
}

async fn run() -> Result<()> {
    // Load OpenAI API key from environment variables
    let api_key = env::var("OPENAI_API_KEY")?;
    println!("Loaded OpenAI API key");

    let url = "wss://api.openai.com/v1/realtime?model=gpt-4o-realtime-preview";
    let mut req = url.into_client_request()?;

    // Add required headers for realtime API
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", api_key).parse()?);
    req.headers_mut()
        .insert("OpenAI-Beta", "realtime=v1".parse()?);

    println!("Connecting to OpenAI Realtime WebSocket...");
    let (ws, _) = connect_async(req).await?;
    println!("Connected to OpenAI Realtime WebSocket");

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
    println!("Microphone capture started");

    // Audio output setup (rodio 0.22.1)
    let handle = DeviceSinkBuilder::open_default_sink()?;
    let player = Player::connect_new(&handle.mixer());
    let player = Arc::new(player);
    let player_clone = player.clone();

    // Writer task with basic silence detection (VAD)
    let writer = tokio::spawn(async move {
        // Load configuration from environment variables or use default values
        let silence_threshold: i16 = env::var("SILENCE_THRESHOLD")
            .unwrap_or_else(|_| "500".to_string())
            .parse()
            .unwrap_or(500); // default: 500

        let silence_frames_required: usize = env::var("SILENCE_FRAMES_REQUIRED")
            .unwrap_or_else(|_| "8".to_string())
            .parse()
            .unwrap_or(8); // default: 8 frames (~400ms)

        println!(
            "Voice activity detection configured: threshold={}, frames_required={}",
            silence_threshold, silence_frames_required
        );

        let mut silence_frames = 0;
        let mut speaking = false;

        while let Some(samples) = rx.recv().await {
            // Calculate average energy of the audio frame
            let avg_energy: i16 =
                samples.iter().map(|s| s.abs()).sum::<i16>() / samples.len() as i16;

            let is_speaking = avg_energy > silence_threshold;

            if is_speaking {
                speaking = true;
                silence_frames = 0;

                // Send audio chunk to the WebSocket buffer
                let bytes: &[u8] = bytemuck::cast_slice(&samples);
                let b64 = STANDARD.encode(bytes);

                let msg = json!({
                    "type": "input_audio_buffer.append",
                    "audio": b64
                });

                let _ = write.send(Message::Text(msg.to_string().into())).await;
                println!("Audio chunk sent (speaking)");
            } else if speaking {
                silence_frames += 1;

                if silence_frames >= silence_frames_required {
                    // Commit the buffer only after prolonged silence
                    let commit_msg = json!({
                        "type": "input_audio_buffer.commit"
                    });

                    let _ = write
                        .send(Message::Text(commit_msg.to_string().into()))
                        .await;

                    println!("Audio committed after silence");

                    speaking = false;
                    silence_frames = 0;
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
                                println!("Audio chunk played");
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

// Spawn microphone capture thread
fn spawn_mic_capture(tx: mpsc::Sender<Vec<i16>>) -> Result<()> {
    std::thread::spawn(move || {
        let host = cpal::default_host();

        // Try to find the virtual TranslatorMic device first
        let device = host
            .input_devices()
            .expect("Failed to list input devices")
            .find(|d| {
                let desc = d.description(); // DeviceDescription
                let desc_str = format!("{:?}", desc); // Convert to String for .contains
                desc_str.contains("translator_mic")
            })
            // Fallback to default input device if virtual mic not found
            .or_else(|| host.default_input_device())
            .expect("No input device available");

        println!(
            "Using input device: {}",
            format!("{:?}", device.description()) // log as string
        );

        let config = device.default_input_config().expect("No default config");

        // Build input stream
        let stream = device
            .build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    if let Err(e) = tx.blocking_send(data.to_vec()) {
                        eprintln!("Failed to send audio samples: {:?}", e);
                    }
                },
                move |err| eprintln!("Microphone error: {:?}", err),
                None,
            )
            .expect("Failed to build input stream");

        // Start streaming audio
        stream.play().expect("Failed to start input stream");

        println!("Microphone capture started");

        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });

    Ok(())
}
