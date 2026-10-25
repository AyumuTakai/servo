/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

//! A crate to hold very common types in Servo.
//!
//! You should almost never need to add a data type to this crate. Instead look for
//! a more shared crate that has fewer dependents.

pub mod cross_process_instant;
pub mod generic_channel;
#[macro_use]
pub mod id;
pub mod print_tree;
mod rope;
pub mod text;
pub mod threadboost;
pub mod threadpool;
mod unicode_block;

use std::fs::File;
use std::io::{BufWriter, Read};
use std::path::Path;

use log::{trace, warn};
use malloc_size_of_derive::MallocSizeOf;
pub use rope::{Rope, RopeChars, RopeIndex, RopeMovement, RopeSlice};
use serde::{Deserialize, Serialize};
use webrender_api::Epoch as WebRenderEpoch;

pub fn read_json_from_file<T>(data: &mut T, config_dir: &Path, filename: &str)
where
    T: for<'de> Deserialize<'de>,
{
    let path = config_dir.join(filename);
    let display = path.display();

    let mut file = match File::open(&path) {
        Err(why) => {
            warn!("couldn't open {}: {}", display, why);
            return;
        },
        Ok(file) => file,
    };

    let mut string_buffer: String = String::new();
    match file.read_to_string(&mut string_buffer) {
        Err(why) => panic!("couldn't read from {}: {}", display, why),
        Ok(_) => trace!("successfully read from {}", display),
    }

    match serde_json::from_str(&string_buffer) {
        Ok(decoded_buffer) => *data = decoded_buffer,
        Err(why) => warn!("Could not decode buffer{}", why),
    }
}

pub fn write_json_to_file<T>(data: &T, config_dir: &Path, filename: &str)
where
    T: Serialize,
{
    let path = config_dir.join(filename);
    let display = path.display();

    let mut file = match File::create(&path) {
        Err(why) => panic!("couldn't create {}: {}", display, why),
        Ok(file) => file,
    };
    let mut writer = BufWriter::new(&mut file);
    serde_json::to_writer_pretty(&mut writer, data).expect("Could not serialize to file");
    trace!("successfully wrote to {display}");
}

/// `data` as pretty-printed JSON (for `write_file_atomically`).
pub fn json_bytes<T: Serialize>(data: &T) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(data).map_err(|error| error.to_string())
}

/// Write `bytes` to `config_dir/filename` so that the file is never seen half written: write a
/// temporary file next to it (readable only by its owner on Unix, since it may hold secrets such as
/// cookies), flush it to disk, then rename it over the old file. Errors are returned, not panicked on.
pub fn write_file_atomically(config_dir: &Path, filename: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let path = config_dir.join(filename);
    let tmp = config_dir.join(format!(".{filename}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A struct for denoting the age of messages; prevents race conditions.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    MallocSizeOf,
)]
pub struct Epoch(pub u32);

impl Epoch {
    pub fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl From<Epoch> for WebRenderEpoch {
    fn from(val: Epoch) -> Self {
        WebRenderEpoch(val.0)
    }
}

pub trait WebRenderEpochToU16 {
    fn as_u16(&self) -> u16;
}

impl WebRenderEpochToU16 for WebRenderEpoch {
    /// The value of this [`Epoch`] as a u16 value. Note that if this Epoch's
    /// value is more than u16::MAX, then the return value will be modulo
    /// u16::MAX.
    fn as_u16(&self) -> u16 {
        (self.0 % u16::MAX as u32) as u16
    }
}
