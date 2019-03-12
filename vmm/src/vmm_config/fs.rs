// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter, Result};
use std::result;

/// This struct represents the strongly typed equivalent of the json body
/// from fs related requests.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsDeviceConfig {
    /// vhost-user socket path.
    pub sock_path: String,
    /// Mount tag used inside the guest.
    pub tag: String,
    /// Number of virtqueues to use.
    pub num_queues: usize,
    /// Size of each virtqueue.
    pub queue_size: u16,
    /// Size of the shared memory cache.
    pub cache_size: usize,
}

/// Errors associated with `FsDeviceConfig`.
#[derive(Debug)]
pub enum FsError {
    /// The update is not allowed after booting the microvm.
    UpdateNotAllowedPostBoot,
}

impl Display for FsError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use self::FsError::*;
        match *self {
            UpdateNotAllowedPostBoot => {
                write!(f, "The update operation is not allowed after boot.",)
            }
        }
    }
}

/// A list with all the fs devices.
pub struct FsDeviceConfigs {
    configs: Vec<FsDeviceConfig>,
}

impl FsDeviceConfigs {
    /// Creates an empty list of FsDeviceConfig.
    pub fn new() -> Self {
        FsDeviceConfigs {
            configs: Vec::new(),
        }
    }

    /// Adds `fs_config` in the list of fs device configurations.
    /// If an entry with the same id already exists, it will update the existing
    /// entry.
    pub fn add(&mut self, cfg: FsDeviceConfig) -> result::Result<(), FsError> {
        match self
            .configs
            .iter()
            .position(|cfg_from_list| cfg_from_list.tag.as_str() == cfg.tag.as_str())
        {
            Some(index) => self.configs[index] = cfg,
            None => self.configs.push(cfg),
        }

        Ok(())
    }

    /// Returns an immutable iterator over the fs available configurations.
    pub fn iter(&mut self) -> ::std::slice::Iter<FsDeviceConfig> {
        self.configs.iter()
    }
}
