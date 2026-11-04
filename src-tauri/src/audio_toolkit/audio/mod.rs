// Re-export all audio components
mod decode;
mod device;
mod mic_sink;
mod microphone;
mod sink;
#[cfg(test)]
pub(crate) mod test_support;
mod utils;
mod visualizer;

pub use decode::decode_audio_file;
pub use device::{list_output_devices, output_device_name, OutputDeviceInfo};
pub use mic_sink::Refused;
pub use microphone::{OpenMicrophone, Sealed, StreamEnd};
pub use sink::{
    AudioFrameCallback, CaptureSink, CaptureVad, CapturedAudio, LevelCallback, VadPolicy,
};
pub use utils::{read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;
