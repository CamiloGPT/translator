# translator

# Linux Virtual Mic

```bash
pactl load-module module-null-sink sink_name=translator_sink sink_properties=device.description=TranslatorSink
pactl load-module module-virtual-source source_name=translator_mic master=translator_sink.monitor source_properties=device.description=TranslatorMic
```
`pavucontrol`

# Run

```bash
export OPENAI_API_KEY=
cargo run --release
curl -X POST http://localhost:8080/start
```
