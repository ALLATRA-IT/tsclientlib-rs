use std::sync::Arc;
use std::time::Duration;

use audiopus::coder::Encoder;
use failure::{format_err, Error};
use futures::prelude::*;
use parking_lot::Mutex;
use sdl2::audio::{
	AudioCallback, AudioDevice, AudioSpec, AudioSpecDesired, AudioStatus,
};
use sdl2::AudioSubsystem;
use slog::{debug, error, o, Logger};
use tokio::runtime::current_thread::Handle;
use tokio::timer::Interval;
use tsclientlib::Connection;
use tsproto::client::ClientConVal;
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

use super::*;

pub struct AudioToTs {
	logger: Logger,
	audio_subsystem: AudioSubsystem,
	executor: Handle,
	listener: Arc<Mutex<Option<ClientConVal>>>,
	device: AudioDevice<SdlCallback>,

	is_playing: bool,
	volume: Arc<Mutex<f32>>,
}

struct SdlCallback {
	logger: Logger,
	spec: AudioSpec,
	encoder: Encoder,
	executor: Handle,
	listener: Arc<Mutex<Option<ClientConVal>>>,
	volume: Arc<Mutex<f32>>,

	opus_output: [u8; MAX_OPUS_FRAME_SIZE],
}

impl AudioToTs {
	pub fn new(
		logger: Logger,
		audio_subsystem: AudioSubsystem,
		executor: Handle,
	) -> Result<Arc<Mutex<Self>>, Error>
	{
		let logger = logger.new(o!("pipeline" => "audio-to-ts"));
		let listener = Arc::new(Mutex::new(Default::default()));
		let volume = Arc::new(Mutex::new(1.0));

		let device = Self::open_capture(
			logger.clone(),
			&audio_subsystem,
			executor.clone(),
			listener.clone(),
			volume.clone(),
		)?;

		let res = Arc::new(Mutex::new(Self {
			logger,
			audio_subsystem,
			executor,
			listener,
			device,

			is_playing: false,
			volume,
		}));

		Self::start(res.clone());

		Ok(res)
	}

	fn open_capture(
		logger: Logger,
		audio_subsystem: &AudioSubsystem,
		executor: Handle,
		listener: Arc<Mutex<Option<ClientConVal>>>,
		volume: Arc<Mutex<f32>>,
	) -> Result<AudioDevice<SdlCallback>, Error>
	{
		let desired_spec = AudioSpecDesired {
			freq: Some(48000),
			channels: Some(1),
			// Default sample size, 20 ms per packet
			samples: Some(48000 / 50),
		};

		audio_subsystem.open_capture(None, &desired_spec, |spec| {
			// This spec will always be the desired spec, the sdl wrapper passes
			// zero as `allowed_changes`.
			debug!(logger, "Got capture spec"; "spec" => ?spec, "driver" => audio_subsystem.current_audio_driver());
			let opus_channels = if spec.channels == 1 {
				audiopus::Channels::Mono
			} else {
				audiopus::Channels::Stereo
			};

			let encoder = Encoder::new(audiopus::SampleRate::Hz48000,
				opus_channels, audiopus::Application::Voip)
				.expect("Could not create encoder");

			SdlCallback {
				logger,
				spec,
				encoder,
				executor,
				listener,
				volume,

				opus_output: [0; MAX_OPUS_FRAME_SIZE],
			}
		}).map_err(|e| format_err!("SDL error: {}", e))
	}

	pub fn set_listener(&self, con: &Connection) {
		let mut listener = self.listener.lock();
		*listener = Some(con.get_tsproto_connection());
	}

	pub fn set_volume(&mut self, volume: f32) { *self.volume.lock() = volume; }

	pub fn set_playing(&mut self, playing: bool) {
		if playing {
			self.device.resume();
		} else {
			self.device.pause();
		}
		self.is_playing = playing;
	}

	fn start(a2t: Arc<Mutex<Self>>) {
		let logger = a2t.lock().logger.clone();
		tokio::runtime::current_thread::spawn(
			Interval::new_interval(Duration::from_secs(1))
				.for_each(move |_| {
					let mut a2t = a2t.lock();
					if a2t.device.status() == AudioStatus::Stopped {
						// Try to reconnect to audio
						match Self::open_capture(
							a2t.logger.clone(),
							&a2t.audio_subsystem,
							a2t.executor.clone(),
							a2t.listener.clone(),
							a2t.volume.clone(),
						) {
							Ok(d) => {
								a2t.device = d;
								debug!(
									a2t.logger,
									"Reconnected to capture device"
								);
								if a2t.is_playing {
									a2t.device.resume();
								}
							}
							Err(e) => {
								error!(a2t.logger, "Failed to open capture device"; "error" => ?e);
							}
						};
					}
					Ok(())
				})
				.map_err(
					move |e| error!(logger, "a2t interval failed"; "error" => ?e),
				),
		);
	}
}

impl AudioCallback for SdlCallback {
	type Channel = f32;
	fn callback(&mut self, buffer: &mut [Self::Channel]) {
		// Handle volume
		let volume = *self.volume.lock();
		if volume != 1.0 {
			for d in &mut *buffer {
				*d *= volume;
			}
		}

		match self.encoder.encode_float(buffer, &mut self.opus_output[..]) {
			Err(e) => {
				error!(self.logger, "Failed to encode opus"; "error" => ?e);
			}
			Ok(len) => {
				// Create packet
				let codec = if self.spec.channels == 1 {
					CodecType::OpusVoice
				} else {
					CodecType::OpusMusic
				};
				let packet = OutAudio::new(&AudioData::C2S {
					id: 0,
					codec,
					data: &self.opus_output[..len],
				});

				// Write into packet sink
				let mut listener = self.listener.lock();
				if let Some(con) = &mut *listener {
					if con.upgrade().is_none() {
						*listener = None;
						return;
					}

					let sink = con.as_packet_sink();
					let logger = self.logger.clone();
					self.executor
						.spawn(sink.send(packet).map(|_| ()).map_err(
							move |e| {
								error!(logger, "Failed to send packet"; "error" => ?e);
							},
						))
						.unwrap();
				}
			}
		}
	}
}
