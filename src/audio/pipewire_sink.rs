//! Native PipeWire audio sink for librespot.
//!
//! librespot-playback has no PipeWire backend of its own, so this implements
//! the [Sink] trait directly on top of pipewire-rs. The PipeWire main loop and
//! stream are `!Send` and therefore live on a dedicated thread; librespot's
//! player thread hands samples over through a wait-free SPSC ring buffer.
//! Output latency is governed solely by the requested quantum, the ring only
//! buffers decoded audio ahead of time.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::config::AudioFormat;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use log::{debug, error, info, warn};
use pipewire as pw;
use pw::spa;

/// Quantum (in frames) requested from PipeWire when the user hasn't
/// configured one. 1024/44100 ≈ 23 ms.
pub const DEFAULT_QUANTUM: u32 = 1024;

/// How many quanta of decoded audio the ring buffer holds. This is
/// decode-ahead buffering between librespot and the PipeWire thread; it does
/// not add output latency.
const RING_QUANTA: usize = 8;

enum PwCommand {
    SetActive(bool),
    Terminate,
}

struct PwThread {
    handle: Option<JoinHandle<()>>,
    commands: pw::channel::Sender<PwCommand>,
    producer: rtrb::Producer<u8>,
}

pub struct PipeWireSink {
    format: AudioFormat,
    device: Option<String>,
    quantum: u32,
    thread: Option<PwThread>,
}

impl PipeWireSink {
    pub fn new(device: Option<String>, format: AudioFormat, quantum: u32) -> Self {
        Self {
            format,
            device,
            quantum,
            thread: None,
        }
    }

    fn bytes_per_frame(format: AudioFormat) -> usize {
        format.size() * NUM_CHANNELS as usize
    }

    fn spa_format(format: AudioFormat) -> SinkResult<spa::param::audio::AudioFormat> {
        match format {
            AudioFormat::S16 => Ok(spa::param::audio::AudioFormat::S16LE),
            AudioFormat::S32 => Ok(spa::param::audio::AudioFormat::S32LE),
            AudioFormat::F32 => Ok(spa::param::audio::AudioFormat::F32LE),
            other => Err(SinkError::InvalidParams(format!(
                "audio format {other:?} is not supported by the pipewire backend, use S16, S32 or F32"
            ))),
        }
    }

    /// Spawn the PipeWire loop thread and wait until its stream is connected.
    fn spawn_thread(&self) -> SinkResult<PwThread> {
        let frame_size = Self::bytes_per_frame(self.format);
        let ring_size = RING_QUANTA * self.quantum as usize * frame_size;
        let (producer, consumer) = rtrb::RingBuffer::new(ring_size);
        let (cmd_tx, cmd_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let format = self.format;
        let device = self.device.clone();
        let quantum = self.quantum;
        let handle = std::thread::Builder::new()
            .name("pipewire-sink".into())
            .spawn(move || pw_thread_main(consumer, cmd_rx, ready_tx, format, device, quantum))
            .map_err(|e| SinkError::ConnectionRefused(e.to_string()))?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(PwThread {
                handle: Some(handle),
                commands: cmd_tx,
                producer,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(SinkError::ConnectionRefused(e))
            }
            Err(_) => Err(SinkError::ConnectionRefused(
                "timed out waiting for the PipeWire stream to connect".into(),
            )),
        }
    }

    fn send_command(&self, cmd: PwCommand) -> SinkResult<()> {
        let thread = self
            .thread
            .as_ref()
            .ok_or_else(|| SinkError::NotConnected("pipewire thread not running".into()))?;
        thread
            .commands
            .send(cmd)
            .map_err(|_| SinkError::NotConnected("pipewire thread died".into()))
    }
}

impl Sink for PipeWireSink {
    fn start(&mut self) -> SinkResult<()> {
        if self.thread.is_none() {
            self.thread = Some(self.spawn_thread()?);
        }
        self.send_command(PwCommand::SetActive(true))
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Cork the stream instead of tearing it down; buffered audio stays in
        // the ring so pause/resume continues seamlessly.
        self.send_command(PwCommand::SetActive(false))
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|e| SinkError::OnWrite(e.to_string()))?;

        let bytes: Vec<u8> = match self.format {
            AudioFormat::S16 => converter
                .f64_to_s16(samples)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect(),
            AudioFormat::S32 => converter
                .f64_to_s32(samples)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect(),
            AudioFormat::F32 => converter
                .f64_to_f32(samples)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect(),
            _ => unreachable!("rejected in spa_format"),
        };

        let quantum_duration =
            Duration::from_micros(self.quantum as u64 * 1_000_000 / SAMPLE_RATE as u64);
        let producer = &mut self
            .thread
            .as_mut()
            .ok_or_else(|| SinkError::NotConnected("pipewire thread not running".into()))?
            .producer;

        let mut written = 0;
        while written < bytes.len() {
            if producer.is_abandoned() {
                return Err(SinkError::NotConnected("pipewire thread died".into()));
            }
            let n = producer.slots().min(bytes.len() - written);
            if n == 0 {
                // Ring is full: block until the PipeWire thread has consumed
                // roughly half a quantum. This is the backpressure that paces
                // librespot's decoder.
                std::thread::sleep(quantum_duration / 2);
                continue;
            }
            if let Ok(chunk) = producer.write_chunk_uninit(n) {
                written += chunk.fill_from_iter(bytes[written..written + n].iter().copied());
            }
        }
        Ok(())
    }
}

impl Drop for PipeWireSink {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.commands.send(PwCommand::Terminate);
            if let Some(handle) = thread.handle {
                let _ = handle.join();
            }
        }
    }
}

/// Body of the dedicated PipeWire thread: owns the main loop and the stream,
/// pulls audio from the ring buffer in the realtime `process` callback.
fn pw_thread_main(
    consumer: rtrb::Consumer<u8>,
    commands: pw::channel::Receiver<PwCommand>,
    ready: mpsc::Sender<Result<(), String>>,
    format: AudioFormat,
    device: Option<String>,
    quantum: u32,
) {
    let result = (|| -> Result<(), pw::Error> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::APP_NAME => "ncspot",
            *pw::keys::NODE_NAME => "ncspot",
            *pw::keys::NODE_DESCRIPTION => "ncspot Spotify client",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => format!("{quantum}/{SAMPLE_RATE}"),
        };
        if let Some(target) = &device {
            props.insert(*pw::keys::TARGET_OBJECT, target.as_str());
        }

        let stream = pw::stream::StreamRc::new(core, "ncspot", props)?;

        let frame_size = PipeWireSink::bytes_per_frame(format);
        let _listener = stream
            .add_local_listener_with_user_data(consumer)
            .state_changed(|_stream, _consumer, old, new| {
                debug!("pipewire stream state changed: {old:?} -> {new:?}");
            })
            .process(move |stream, consumer| {
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    let requested_frames = buffer.requested() as usize;
                    let data = &mut buffer.datas_mut()[0];
                    let Some(slice) = data.data() else { return };

                    let mut max_bytes = slice.len() - slice.len() % frame_size;
                    if requested_frames > 0 {
                        max_bytes = max_bytes.min(requested_frames * frame_size);
                    }
                    let available = consumer.slots();
                    let filled = available.min(max_bytes);
                    let filled = filled - filled % frame_size;

                    if let Ok(chunk) = consumer.read_chunk(filled) {
                        let (a, b) = chunk.as_slices();
                        slice[..a.len()].copy_from_slice(a);
                        slice[a.len()..a.len() + b.len()].copy_from_slice(b);
                        chunk.commit_all();
                    }
                    if filled < max_bytes {
                        // Underrun: keep the stream timing intact with silence.
                        slice[filled..max_bytes].fill(0);
                    }

                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = frame_size as _;
                    *chunk.size_mut() = max_bytes as _;
                }
            })
            .register()?;

        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(PipeWireSink::spa_format(format).map_err(|e| {
            error!("{e}");
            pw::Error::CreationFailed
        })?);
        audio_info.set_rate(SAMPLE_RATE);
        audio_info.set_channels(NUM_CHANNELS as u32);
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        audio_info.set_position(position);

        let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(spa::pod::Object {
                type_: spa::sys::SPA_TYPE_OBJECT_Format,
                id: spa::sys::SPA_PARAM_EnumFormat,
                properties: audio_info.into(),
            }),
        )
        .map_err(|_| pw::Error::CreationFailed)?
        .0
        .into_inner();
        let mut params = [spa::pod::Pod::from_bytes(&values).ok_or(pw::Error::CreationFailed)?];

        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;

        let _cmd_receiver = commands.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            let stream = stream.clone();
            move |cmd| match cmd {
                PwCommand::SetActive(active) => {
                    if let Err(e) = stream.set_active(active) {
                        warn!("failed to set pipewire stream active={active}: {e}");
                    }
                }
                PwCommand::Terminate => mainloop.quit(),
            }
        });

        info!("pipewire stream connected (quantum {quantum}/{SAMPLE_RATE})");
        let _ = ready.send(Ok(()));
        mainloop.run();
        Ok(())
    })();

    if let Err(e) = result {
        error!("pipewire sink failed: {e}");
        let _ = ready.send(Err(e.to_string()));
    }
    debug!("pipewire sink thread exited");
}
