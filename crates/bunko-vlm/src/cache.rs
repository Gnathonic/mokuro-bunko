//! One loaded recognizer per (engine, assets, device, precision, session options): the
//! "N engine copies become N threads over one session" rule of ARCHITECTURE §3.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use crate::hayai::{HayaiAssets, HayaiNova};
use crate::paddle::{PaddleAssets, PaddleManga};
use crate::runtime::{SessionFactory, SessionOptions};
use crate::{Recognizer, VlmError};

/// What identifies a loaded recognizer. Precision is implied by the asset files.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EngineKey {
    Hayai {
        assets: HayaiAssets,
        opts: SessionOptions,
        budget: usize,
    },
    Paddle {
        assets: PaddleAssets,
        opts: SessionOptions,
    },
}

/// Hands out shared recognizers; a recognizer is dropped when its last user lets go.
pub struct RecognizerCache {
    factory: Arc<dyn SessionFactory>,
    loaded: Mutex<HashMap<EngineKey, Weak<dyn Recognizer>>>,
}

impl RecognizerCache {
    pub fn new(factory: Arc<dyn SessionFactory>) -> Self {
        Self {
            factory,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// The recognizer for `key`, loading it on first use. Loads are serialised (they
    /// are rare and memory-heavy; two at once would only compete for RAM).
    pub fn get(&self, key: &EngineKey) -> Result<Arc<dyn Recognizer>, VlmError> {
        let mut map = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = map.get(key).and_then(Weak::upgrade) {
            return Ok(r);
        }
        let r: Arc<dyn Recognizer> = match key {
            EngineKey::Hayai {
                assets,
                opts,
                budget,
            } => Arc::new(HayaiNova::load(&*self.factory, assets, opts, *budget)?),
            EngineKey::Paddle { assets, opts } => {
                Arc::new(PaddleManga::load(&*self.factory, assets, opts)?)
            }
        };
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(key.clone(), Arc::downgrade(&r));
        Ok(r)
    }
}
