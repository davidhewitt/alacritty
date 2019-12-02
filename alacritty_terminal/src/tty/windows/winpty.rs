// Copyright 2016 Joe Wilm, The Alacritty Project Contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{FromRawHandle, IntoRawHandle};
use std::sync::Arc;
use std::u16;

use dunce::canonicalize;
use log::info;
use mio_named_pipes::NamedPipe;
use winapi::um::winbase::FILE_FLAG_OVERLAPPED;
use winpty::{Config as WinptyConfig, ConfigFlags, MouseMode, SpawnConfig, SpawnFlags, Winpty};

use crate::config::{Config, Shell};
use crate::event::OnResize;
use crate::term::SizeInfo;
use crate::tty::windows::child::ChildExitWatcher;
use crate::tty::windows::{Pty, PtyImpl};

pub struct WinptyAgent {
    winpty: Arc<Winpty>,
    conout: NamedPipe,
    conin: NamedPipe,
}

impl PtyImpl for WinptyAgent {
    type ResizeHandle = WinptyResizeHandle;
    type Conout = NamedPipe;
    type Conin = NamedPipe;

    fn resize_handle(&self) -> Self::ResizeHandle {
        WinptyResizeHandle { winpty: self.winpty.clone() }
    }

    fn conout(&self) -> &Self::Conout {
        &self.conout
    }

    fn conout_mut(&mut self) -> &mut Self::Conout {
        &mut self.conout
    }

    fn conin(&self) -> &Self::Conin {
        &self.conin
    }

    fn conin_mut(&mut self) -> &mut Self::Conin {
        &mut self.conin
    }
}

impl WinptyAgent {
    pub fn new(winpty: Winpty, conout: NamedPipe, conin: NamedPipe) -> Self {
        Self { winpty: Arc::new(winpty), conout, conin }
    }
}

/// Resize handle to safely move between threads.
pub struct WinptyResizeHandle {
    // Doesn't need the in/out pipes here!
    winpty: Arc<Winpty>,
}

impl OnResize for WinptyResizeHandle {
    fn on_resize(&mut self, size: &SizeInfo) {
        let (cols, lines) = (size.cols().0, size.lines().0);
        if cols > 0 && cols <= u16::MAX as usize && lines > 0 && lines <= u16::MAX as usize {
            let winpty: &mut Winpty = unsafe {
                // This transmute is actually thread-safe since Winpty uses a mutex internally.
                std::mem::transmute(&self.winpty as *const _ as *mut Winpty)
            };

            winpty
                .set_size(cols as u16, lines as u16)
                .unwrap_or_else(|_| info!("Unable to set winpty size, did it die?"));
        }
    }
}

/// How long the winpty agent should wait for any RPC request
/// This is a placeholder value until we see how often long responses happen
const AGENT_TIMEOUT: u32 = 10000;

pub fn new<C>(config: &Config<C>, size: &SizeInfo, _window_id: Option<usize>) -> Pty<WinptyAgent> {
    // Create config
    let mut wconfig = WinptyConfig::new(ConfigFlags::empty()).unwrap();

    wconfig.set_initial_size(size.cols().0 as i32, size.lines().0 as i32);
    wconfig.set_mouse_mode(&MouseMode::Auto);
    wconfig.set_agent_timeout(AGENT_TIMEOUT);

    // Start agent
    let mut winpty = Winpty::open(&wconfig).unwrap();
    let (conin, conout) = (winpty.conin_name(), winpty.conout_name());

    // Get process commandline
    let default_shell = &Shell::new("powershell");
    let shell = config.shell.as_ref().unwrap_or(default_shell);
    let mut cmdline = shell.args.clone();
    cmdline.insert(0, shell.program.to_string());

    // Warning, here be borrow hell
    let cwd = config.working_directory().as_ref().map(|dir| canonicalize(dir).unwrap());
    let cwd = cwd.as_ref().map(|dir| dir.to_str().unwrap());

    // Spawn process
    let spawnconfig = SpawnConfig::new(
        SpawnFlags::AUTO_SHUTDOWN | SpawnFlags::EXIT_AFTER_SHUTDOWN,
        None, // appname
        Some(&cmdline.join(" ")),
        cwd,
        None, // Env
    )
    .unwrap();

    let default_opts = &mut OpenOptions::new();
    default_opts.share_mode(0).custom_flags(FILE_FLAG_OVERLAPPED);

    let (conout_pipe, conin_pipe);
    unsafe {
        conout_pipe = NamedPipe::from_raw_handle(
            default_opts.clone().read(true).open(conout).unwrap().into_raw_handle(),
        );
        conin_pipe = NamedPipe::from_raw_handle(
            default_opts.clone().write(true).open(conin).unwrap().into_raw_handle(),
        );
    };

    if let Some(err) = conout_pipe.connect().err() {
        if err.kind() != io::ErrorKind::WouldBlock {
            panic!(err);
        }
    }
    assert!(conout_pipe.take_error().unwrap().is_none());

    if let Some(err) = conin_pipe.connect().err() {
        if err.kind() != io::ErrorKind::WouldBlock {
            panic!(err);
        }
    }
    assert!(conin_pipe.take_error().unwrap().is_none());

    winpty.spawn(&spawnconfig).unwrap();

    let child_watcher = ChildExitWatcher::new(winpty.raw_handle()).unwrap();
    let agent = WinptyAgent::new(winpty, conout_pipe, conin_pipe);

    Pty::new(agent, child_watcher)
}
