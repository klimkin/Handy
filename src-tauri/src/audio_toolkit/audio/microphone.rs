use std::sync::{Arc, OnceLock};

use handy_recorder::{EndReason, Error, ErrorKind, Recorder, RecorderConfig, StartError};

use super::mic_sink::{Inbox, MicSink, Refused};
use super::CaptureSink;

/// An open microphone. Its stream is armed for each recording and, while it
/// keeps listening, between recordings too, holding the pre-roll for the
/// next one. Dropping the last handle closes the device.
pub struct OpenMicrophone {
    recorder: Recorder<MicSink>,
    inbox: Inbox,
    /// Per microphone, so a failure handler that runs late can't mark a
    /// replacement failed.
    failure: Arc<OnceLock<Error>>,
}

/// What [`OpenMicrophone::seal`] hands back.
pub struct Sealed {
    /// The posted recording, with the final chunk; `None` if none was posted
    /// or a failed stop lost it with the sink.
    pub recording: Option<CaptureSink>,
    pub end: Result<StreamEnd, Error>,
}

/// How an armed stream ended.
pub struct StreamEnd {
    pub reason: EndReason,
    /// Frames the recorder dropped since the stream was armed, at the
    /// device's rate. In always-on mode that includes the wait before the
    /// press.
    pub dropped_frames: u64,
}

impl OpenMicrophone {
    pub fn open(config: RecorderConfig) -> Result<Self, Error> {
        Self::open_with(|on_failure| Recorder::open_with_failure_handler(config, on_failure))
    }

    pub(super) fn open_with(
        open: impl FnOnce(Box<dyn FnOnce(Error) + Send>) -> Result<Recorder<MicSink>, Error>,
    ) -> Result<Self, Error> {
        let failure = Arc::new(OnceLock::new());
        // handy-recorder logs the failure.
        let recorder = open(Box::new({
            let failure = Arc::clone(&failure);
            move |error| {
                let _ = failure.set(error);
            }
        }))?;
        Ok(Self {
            inbox: Inbox::new(recorder.info().frames_per_chunk),
            recorder,
            failure,
        })
    }

    pub fn needs_reopen(&self) -> bool {
        self.failure.get().is_some()
    }

    fn fail(&self, error: &Error) {
        let _ = self.failure.set(error.clone());
    }

    /// Hands `recording` the stream's audio from shortly before now on, once
    /// the stream is armed.
    pub fn post(&self, recording: CaptureSink) -> Result<(), Refused> {
        self.inbox.post(recording)
    }

    /// Starts the stream unless it is armed. Errs if the recorder has failed,
    /// armed or not, and every error leaves `needs_reopen` true.
    pub fn arm(&self) -> Result<(), Error> {
        match self.recorder.start(MicSink::new(self.inbox.clone())) {
            Err(StartError { error, .. }) if error.kind() != ErrorKind::AlreadyRecording => {
                self.fail(&error);
                Err(error)
            }
            _ => self
                .failure
                .get()
                .map_or(Ok(()), |error| Err(error.clone())),
        }
    }

    /// Stops the stream and hands back its recording with the final chunk.
    /// The device stays open, unarmed.
    pub fn seal(&self) -> Sealed {
        match self.recorder.stop() {
            Ok(stopped) => {
                if let EndReason::RecorderFailed(error) = &stopped.end_reason {
                    self.fail(error);
                }
                Sealed {
                    recording: stopped.sink.into_recording(),
                    end: Ok(StreamEnd {
                        reason: stopped.end_reason,
                        dropped_frames: stopped.dropped_frames,
                    }),
                }
            }
            Err(error) => {
                if error.kind() != ErrorKind::NotRecording {
                    self.fail(&error);
                }
                Sealed {
                    recording: self.inbox.reclaim(),
                    end: Err(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
