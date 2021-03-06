use board::boot;
use std::io::{self, Read, Write};
use log::{self, LevelFilter};
use logger_artiq::BufferLogger;
use sched::Io;
use sched::{TcpListener, TcpStream};
use proto::WriteExt;
use mgmt_proto::*;
use profiler;

fn check_magic(stream: &mut TcpStream) -> io::Result<()> {
    const MAGIC: &'static [u8] = b"ARTIQ management\n";

    let mut magic: [u8; 17] = [0; 17];
    stream.read_exact(&mut magic)?;
    if magic != MAGIC {
        Err(io::Error::new(io::ErrorKind::InvalidData, "unrecognized magic"))
    } else {
        Ok(())
    }
}

fn worker(io: &Io, stream: &mut TcpStream) -> io::Result<()> {
    check_magic(stream)?;
    info!("new connection from {}", stream.remote_endpoint());

    loop {
        match Request::read_from(stream)? {
            Request::GetLog => {
                BufferLogger::with(|logger| {
                    let mut buffer = io.until_ok(|| logger.buffer())?;
                    Reply::LogContent(buffer.extract()).write_to(stream)
                })?;
            }

            Request::ClearLog => {
                BufferLogger::with(|logger| -> io::Result<()> {
                    let mut buffer = io.until_ok(|| logger.buffer())?;
                    Ok(buffer.clear())
                })?;

                Reply::Success.write_to(stream)?;
            }

            Request::PullLog => {
                BufferLogger::with(|logger| -> io::Result<()> {
                    loop {
                        // Do this *before* acquiring the buffer, since that sets the log level
                        // to OFF.
                        let log_level = log::max_level();

                        let mut buffer = io.until_ok(|| logger.buffer())?;
                        if buffer.is_empty() { continue }

                        stream.write_string(buffer.extract())?;

                        if log_level == LevelFilter::Trace {
                            // Hold exclusive access over the logger until we get positive
                            // acknowledgement; otherwise we get an infinite loop of network
                            // trace messages being transmitted and causing more network
                            // trace messages to be emitted.
                            //
                            // Any messages unrelated to this management socket that arrive
                            // while it is flushed are lost, but such is life.
                            stream.flush()?;
                        }

                        // Clear the log *after* flushing the network buffers, or we're just
                        // going to resend all the trace messages on the next iteration.
                        buffer.clear();
                    }
                })?;
            }

            Request::SetLogFilter(level) => {
                info!("changing log level to {}", level);
                log::set_max_level(level);
                Reply::Success.write_to(stream)?;
            }

            Request::SetUartLogFilter(level) => {
                info!("changing UART log level to {}", level);
                BufferLogger::with(|logger|
                    logger.set_uart_log_level(level));
                Reply::Success.write_to(stream)?;
            }

            Request::StartProfiler { interval_us, hits_size, edges_size } => {
                match profiler::start(interval_us as u64,
                                      hits_size as usize, edges_size as usize) {
                    Ok(()) => Reply::Success.write_to(stream)?,
                    Err(()) => Reply::Unavailable.write_to(stream)?
                }
            }

            Request::StopProfiler => {
                profiler::stop();
                Reply::Success.write_to(stream)?;
            }

            Request::GetProfile => {
                profiler::pause(|profile| {
                    let profile = match profile {
                        None => return Reply::Unavailable.write_to(stream),
                        Some(profile) => profile
                    };

                    Reply::Profile.write_to(stream)?;
                    {
                        let hits = profile.hits();
                        stream.write_u32(hits.len() as u32)?;
                        for (&addr, &count) in hits.iter() {
                            stream.write_u32(addr.as_raw() as u32)?;
                            stream.write_u32(count)?;
                        }
                    }
                    {
                        let edges = profile.edges();
                        stream.write_u32(edges.len() as u32)?;
                        for (&(caller, callee), &count) in edges.iter() {
                            stream.write_u32(caller.as_raw() as u32)?;
                            stream.write_u32(callee.as_raw() as u32)?;
                            stream.write_u32(count)?;
                        }
                    }

                    Ok(())
                })?;
            }

            Request::Hotswap(firmware) => {
                Reply::RebootImminent.write_to(stream)?;
                stream.close()?;
                stream.flush()?;

                profiler::stop();
                warn!("hotswapping firmware");
                unsafe { boot::hotswap(&firmware) }
            }

            Request::Reboot => {
                Reply::RebootImminent.write_to(stream)?;
                stream.close()?;
                stream.flush()?;

                profiler::stop();
                warn!("restarting");
                unsafe { boot::reset() }
            }

            Request::DebugAllocator =>
                unsafe { println!("{}", ::ALLOC) },
        };
    }
}

pub fn thread(io: Io) {
    let listener = TcpListener::new(&io, 8192);
    listener.listen(1380).expect("mgmt: cannot listen");
    info!("management interface active");

    loop {
        let stream = listener.accept().expect("mgmt: cannot accept").into_handle();
        io.spawn(4096, move |io| {
            let mut stream = TcpStream::from_handle(&io, stream);
            match worker(&io, &mut stream) {
                Ok(()) => (),
                Err(ref err) if err.kind() == io::ErrorKind::UnexpectedEof => (),
                Err(ref err) if err.kind() == io::ErrorKind::WriteZero => (),
                Err(err) => error!("aborted: {}", err)
            }
        });
    }
}
