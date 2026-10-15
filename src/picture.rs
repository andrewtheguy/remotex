//! What [`crate::encode`]'s sink encodes: the desktop's one VP9 stream, or a picture
//! sent for motion — PNG where it is still, VP9 for each region that moves.
//!
//! Both encode their video a round at a time on a blocking worker, so the sink
//! drives the rounds of either the same way; this is that one way. Damage and the
//! cleanups of a picture sent for motion are the sink's to send
//! ([`Picture::motion`]).

use tokio::time::Instant;

use crate::config::RenderPlan;
use crate::motion::{MotionRound, MotionStream};
use crate::shadow::Rect;
use crate::stream::{DesktopStream, Produced, Round};

pub enum Picture {
    Desktop(DesktopStream),
    Motion(MotionStream),
}

impl Picture {
    /// The picture `plan` asks for.
    pub fn new(plan: RenderPlan) -> Self {
        if plan.motion {
            Self::Motion(MotionStream::new(plan.quality, plan.native_vp9, plan.motion_debug))
        } else {
            Self::Desktop(DesktopStream::new(plan.quality, plan.native_vp9))
        }
    }

    pub fn want(&mut self, w: u16, h: u16) {
        match self {
            Self::Desktop(stream) => stream.want(w, h),
            Self::Motion(stream) => stream.want(w, h),
        }
    }

    /// Copy a changed rectangle into the desktop's mirror. A picture sent for
    /// motion takes its damage through [`Self::motion`] instead.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        match self {
            Self::Desktop(stream) => stream.blit(rect, rgb),
            Self::Motion(_) => anyhow::bail!("a picture sent for motion takes its damage as pieces to send"),
        }
    }

    /// The picture sent for motion, where this is one.
    pub fn motion(&mut self) -> Option<&mut MotionStream> {
        match self {
            Self::Desktop(_) => None,
            Self::Motion(stream) => Some(stream),
        }
    }

    pub fn round_out(&self) -> bool {
        match self {
            Self::Desktop(stream) => stream.round_out(),
            Self::Motion(stream) => stream.round_out(),
        }
    }

    /// Whether the picture holds something no round has carried yet.
    pub fn dirty(&self) -> bool {
        match self {
            Self::Desktop(stream) => stream.dirty(),
            Self::Motion(stream) => stream.dirty(),
        }
    }

    pub fn take_round(&mut self) -> anyhow::Result<Option<PictureRound>> {
        Ok(match self {
            Self::Desktop(stream) => stream.take_round()?.map(|round| PictureRound::Desktop(Box::new(round))),
            Self::Motion(stream) => stream.take_round(Instant::now())?.map(|round| PictureRound::Motion(Box::new(round))),
        })
    }

    pub fn put_back(&mut self, round: PictureRound) {
        match (self, round) {
            (Self::Desktop(stream), PictureRound::Desktop(round)) => stream.put_back(*round),
            (Self::Motion(stream), PictureRound::Motion(round)) => stream.put_back(*round, Instant::now()),
            _ => unreachable!("a round is put back into the picture it was taken from"),
        }
    }

    pub fn set_quality(&mut self, quality: u8) {
        match self {
            Self::Desktop(stream) => stream.set_quality(quality),
            Self::Motion(stream) => stream.set_quality(quality),
        }
    }

    pub fn quality(&self) -> u8 {
        match self {
            Self::Desktop(stream) => stream.quality(),
            Self::Motion(stream) => stream.quality(),
        }
    }

    /// Sharpen a picture that went quiet below the dial. A picture sent for motion
    /// has nothing to sharpen: what is still on it is PNG.
    pub fn settle(&mut self, quality: u8) {
        if let Self::Desktop(stream) = self {
            stream.settle(quality);
        }
    }

    pub fn force_keyframe(&mut self) {
        match self {
            Self::Desktop(stream) => stream.force_keyframe(),
            Self::Motion(stream) => stream.force_keyframe(),
        }
    }
}

/// One round of either picture, away on a blocking worker. Boxed: a desktop round
/// carries its mirror and its encoder by value, and the enum is moved through the
/// sink's queue.
pub enum PictureRound {
    Desktop(Box<Round>),
    Motion(Box<MotionRound>),
}

impl PictureRound {
    pub fn keyframe(&self) -> bool {
        match self {
            Self::Desktop(round) => round.keyframe(),
            Self::Motion(round) => round.keyframe(),
        }
    }

    pub fn quality(&self) -> u8 {
        match self {
            Self::Desktop(round) => round.quality(),
            Self::Motion(round) => round.quality(),
        }
    }

    pub fn settling(&self) -> bool {
        match self {
            Self::Desktop(round) => round.settling(),
            Self::Motion(_) => false,
        }
    }

    /// The coarsest any of the client's picture will be once this round is
    /// encoded. A picture sent for motion is at the dial where it moves and exact
    /// where it is still.
    pub fn coarsest(&self) -> u8 {
        match self {
            Self::Desktop(round) => round.coarsest(),
            Self::Motion(round) => round.quality(),
        }
    }

    pub fn left(&self) -> u8 {
        match self {
            Self::Desktop(round) => round.left(),
            Self::Motion(round) => round.quality(),
        }
    }

    pub fn skipped(&self) -> u64 {
        match self {
            Self::Desktop(round) => round.skipped(),
            Self::Motion(round) => round.skipped(),
        }
    }

    /// Encode the round. Blocking: call it on a worker.
    pub fn encode(&mut self) -> anyhow::Result<Produced> {
        match self {
            Self::Desktop(round) => round.encode(),
            Self::Motion(round) => {
                Ok(Produced { format: None, unit: None, records: round.encode()? })
            }
        }
    }
}
