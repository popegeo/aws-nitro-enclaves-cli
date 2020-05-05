// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
#![deny(warnings)]

pub mod commands;
pub mod connection;
pub mod connection_listener;
pub mod cpu_info;
pub mod json_output;
pub mod resource_manager;
pub mod socket;
pub mod utils;

use log::{info, warn};
use nix::sys::signal::{Signal, SIGHUP};
use nix::unistd::*;
use procinfo::pid;
use serde::de::DeserializeOwned;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process;
use std::thread::{self, JoinHandle};

use super::common::MSG_ENCLAVE_CONFIRM;
use super::common::{
    enclave_proc_command_send_single, read_u64_le, receive_command_type, write_u64_le,
};
use super::common::{EnclaveProcessCommandType, ExitGracefully, NitroCliResult};
use crate::common::commands_parser::{EmptyArgs, RunEnclavesArgs};
use crate::common::logger::EnclaveProcLogWriter;
use crate::common::signal_handler::SignalHandler;

use commands::{describe_enclaves, run_enclaves, terminate_enclaves};
use connection::Connection;
use connection_listener::ConnectionListener;
use resource_manager::EnclaveManager;

/// Read the arguments of the CLI command.
fn receive_command_args<T>(input_stream: &mut dyn Read) -> io::Result<T>
where
    T: DeserializeOwned,
{
    let arg_size = read_u64_le(input_stream)? as usize;
    let mut arg_data: Vec<u8> = vec![0; arg_size];
    input_stream.read_exact(&mut arg_data[..])?;
    let args: T = serde_cbor::from_slice(&arg_data[..])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(args)
}

/// Route STDOUT and STDERR also to the CLI socket. Also provide
/// the old (common) descriptor used previously by both.
fn route_output_to(fd: RawFd) -> RawFd {
    let old_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };

    // This effectively enables all messages generated by
    // "print!", "eprint!" etc. to be routed back to the CLI.
    unsafe { libc::dup2(fd, libc::STDOUT_FILENO) };
    unsafe { libc::dup2(fd, libc::STDERR_FILENO) };
    old_fd
}

/// Run a function, routing its output. Output redirection and restoration are
/// performed irrespective of the function's status.
fn safe_route_output<T, R>(
    args: &mut T,
    connection_fd: RawFd,
    func: fn(&mut T) -> NitroCliResult<R>,
) -> NitroCliResult<R> {
    let output_fd = route_output_to(connection_fd);
    let status = func(args);
    route_output_to(output_fd);
    status
}

/// Obtain the logger ID from the full enclave ID.
fn get_logger_id(enclave_id: &str) -> String {
    // The full enclave ID is "i-(...)-enc<enc_id>" and we want to extract only <enc_id>.
    let tokens: Vec<_> = enclave_id.rsplit("-enc").collect();
    format!("enc-{}", tokens[0])
}

fn send_command_and_close(cmd: &EnclaveProcessCommandType, stream: &mut UnixStream) {
    enclave_proc_command_send_single::<EmptyArgs>(cmd, None, stream)
        .ok_or_exit("Failed to send command.");
    stream
        .shutdown(std::net::Shutdown::Both)
        .ok_or_exit("Failed to shut down stream.");
}

/// Perform enclave termination.
fn run_terminate(
    connection: Connection,
    mut thread_stream: UnixStream,
    mut enclave_manager: EnclaveManager,
) {
    safe_route_output(
        &mut enclave_manager,
        connection.as_raw_fd(),
        |mut enclave_manager| terminate_enclaves(&mut enclave_manager),
    )
    .ok_or_exit("Failed to terminate enclave.");

    // Notify the main thread that enclave termination has completed.
    send_command_and_close(
        &EnclaveProcessCommandType::TerminateComplete,
        &mut thread_stream,
    );
}

/// Start enclave termination.
fn notify_terminate(
    connection: Connection,
    conn_listener: &ConnectionListener,
    enclave_manager: EnclaveManager,
) -> Option<JoinHandle<()>> {
    let (local_stream, thread_stream) =
        UnixStream::pair().ok_or_exit("Failed to create stream pair.");

    conn_listener.add_stream_to_epoll(local_stream);
    Some(thread::spawn(move || {
        run_terminate(connection, thread_stream, enclave_manager)
    }))
}

fn enclave_proc_configure_signal_handler(conn_listener: &ConnectionListener) {
    let mut signal_handler = SignalHandler::new_with_defaults().mask_all();
    let (local_stream, thread_stream) =
        UnixStream::pair().ok_or_exit("Failed to create stream pair.");

    conn_listener.add_stream_to_epoll(local_stream);
    signal_handler.start_handler(thread_stream.into_raw_fd(), enclave_proc_handle_signals);
}

fn enclave_proc_handle_signals(comm_fd: RawFd, signal: Signal) -> bool {
    let mut stream = unsafe { UnixStream::from_raw_fd(comm_fd) };

    warn!(
        "Received signal {:?}. The enclave process will now close.",
        signal
    );
    send_command_and_close(
        &EnclaveProcessCommandType::ConnectionListenerStop,
        &mut stream,
    );

    true
}

/// The main event loop of the enclave process.
fn process_event_loop(comm_stream: UnixStream, logger: &EnclaveProcLogWriter) {
    let mut conn_listener = ConnectionListener::new();
    let mut enclave_manager = EnclaveManager::default();
    let mut terminate_thread: Option<std::thread::JoinHandle<()>> = None;

    // Start the signal handler before spawning any other threads. This is done since the
    // handler will mask all relevant signals from the current thread and this setting will
    // be automatically inherited by all threads spawned from this point on; we want this
    // because only the dedicated thread spawned by the handler should listen for signals.
    enclave_proc_configure_signal_handler(&conn_listener);

    // Add the CLI communication channel to epoll.
    conn_listener.handle_new_connection(comm_stream);

    loop {
        // We can get connections to CLI instances, to the resource driver or to ourselves.
        let mut connection = Connection::new(conn_listener.get_epoll_fd());
        let cmd =
            receive_command_type(connection.as_reader()).ok_or_exit("Failed to receive command.");
        info!("Received command: {:?}", cmd);

        match cmd {
            EnclaveProcessCommandType::Run => {
                let mut run_args = receive_command_args::<RunEnclavesArgs>(connection.as_reader())
                    .ok_or_exit("Failed to get run arguments.");
                info!("Run args = {:?}", run_args);

                enclave_manager =
                    safe_route_output(&mut run_args, connection.as_raw_fd(), |mut run_args| {
                        run_enclaves(&mut run_args)
                    })
                    .ok_or_exit("Failed to run enclave.");

                info!("Enclave ID = {}", enclave_manager.enclave_id);
                logger.update_logger_id(&get_logger_id(&enclave_manager.enclave_id));
                conn_listener
                    .start(&enclave_manager.enclave_id)
                    .ok_or_exit("Failed to start connection listener.");

                // TODO: run_enclaves(run_args).ok_or_exit(args.usage());
            }

            EnclaveProcessCommandType::Terminate => {
                terminate_thread =
                    notify_terminate(connection, &conn_listener, enclave_manager.clone());

                //TODO: terminate_enclaves(terminate_args).ok_or_exit(args.usage());
            }

            EnclaveProcessCommandType::TerminateComplete => {
                info!("Enclave has completed termination.");
                match terminate_thread {
                    Some(handle) => handle
                        .join()
                        .ok_or_exit("Failed to retrieve termination thread."),
                    None => warn!("Received termination confirmation on an invalid thread handle."),
                };

                break;
            }

            EnclaveProcessCommandType::GetEnclaveCID => {
                let enclave_cid = enclave_manager
                    .get_console_resources()
                    .ok_or_exit("Failed to get enclave CID.");
                write_u64_le(connection.as_writer(), enclave_cid)
                    .ok_or_exit("Failed to send enclave CID.");
            }

            EnclaveProcessCommandType::Describe => {
                write_u64_le(connection.as_writer(), MSG_ENCLAVE_CONFIRM)
                    .ok_or_exit("Failed to write confirmation.");

                safe_route_output(
                    &mut enclave_manager,
                    connection.as_raw_fd(),
                    |mut enclave_manager| describe_enclaves(&mut enclave_manager),
                )
                .ok_or_exit("Failed to describe enclave.");

                //TODO: describe_enclaves(describe_args).ok_or_exit(args.usage());
            }

            EnclaveProcessCommandType::ConnectionListenerStop => break,
        };
    }

    info!("Enclave process {} exited event loop.", process::id());
    conn_listener.stop();
}

/// Redirect STDIN, STDOUT and STDERR to "/dev/null"
fn hide_standard_descriptors() {
    let null_fd = OpenOptions::new()
        .read(true)
        .write(true)
        .append(true)
        .open("/dev/null")
        .ok_or_exit("Failed to open '/dev/null'")
        .into_raw_fd();
    unsafe { libc::dup2(null_fd, libc::STDIN_FILENO) };
    unsafe { libc::dup2(null_fd, libc::STDOUT_FILENO) };
    unsafe { libc::dup2(null_fd, libc::STDERR_FILENO) };
}

/// Create the enclave process.
fn create_enclave_process() {
    // To get a detached process, we first:
    // (1) Temporarily ignore specific signals (SIGHUP).
    // (2) Fork a child process.
    // (3) Terminate the parent (at which point the child becomes orphaned).
    // (4) Restore signal handlers.
    let signal_handler = SignalHandler::new(&[SIGHUP]).mask_all();

    // We need to redirect the standard descriptors to "/dev/null" in the
    // intermediate process since we want its child (the detached enclave
    // process) to not have terminal access.
    hide_standard_descriptors();

    // The current process must first become session leader.
    setsid().ok_or_exit("setsid() failed.");

    match fork() {
        Ok(ForkResult::Parent { child }) => {
            info!("Parent = {} with child = {:?}", process::id(), child);
            process::exit(0);
        }
        Ok(ForkResult::Child) => {
            // This is our detached process.
            info!("Enclave process PID: {}", process::id());
        }
        Err(e) => panic!("Failed to create child: {}", e),
    }

    // The detached process is not a session leader and thus cannot attach
    // to a terminal. Next, we must wait until we're 100% orphaned.
    loop {
        let stat = pid::stat_self().ok_or_exit("Failed to get process stat.");
        if stat.ppid == 1 {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }

    // Restore signal handlers.
    signal_handler.unmask_all();
}

/// Launch the enclave process.
///
/// * `comm_fd` - A descriptor used for initial communication with the parent Nitro CLI instance.
/// * `logger` - The current log writer, whose ID gets updated when an enclave is launched.
pub fn enclave_process_run(comm_stream: UnixStream, logger: &EnclaveProcLogWriter) -> i32 {
    logger.update_logger_id("enc-xxxxxxxxxxxx");
    create_enclave_process();
    process_event_loop(comm_stream, logger);

    0
}
