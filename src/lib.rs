/// An easy to use logging implementation that is very lightweight
/// Using this crate, you can easily set up multiple different loggers in the same project,
/// implement different log formats, and custom logging outputs
///
pub mod formatters;

use std::fmt::{Display};
use std::io::Error;
use std::fs::{OpenOptions};
pub use std::{fs::File, io::Stderr};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::io::{stderr, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread::{JoinHandle, spawn};
use chrono::Local;
use colored::{Colorize};
pub use lazy_static::lazy_static;
use crate::formatters::LogData;


/// A standard set of log severities
#[derive(Eq, Clone, Copy, PartialEq, Hash, Debug)]
pub enum LogLevel {
	Info,
	Warn,
	Error,
	Trace,
	Debug
}


impl Display for LogLevel {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			LogLevel::Info => write!(f, "INFO"),
			LogLevel::Warn => write!(f, "WARN"),
			LogLevel::Error => write!(f, "ERROR"),
			LogLevel::Trace => write!(f, "TRACE"),
			LogLevel::Debug => write!(f, "DEBUG"),
		}
	}
}


/// A fn that converts a log data into a string
type Formatter<T> = fn(LogData<T>) -> String;


/// Trait for types that can write log data to some output.
/// The type T represents the type of a log severity
pub trait LogDump<T: Eq + Send + Hash + Clone + 'static> {
	fn set_formatter(&mut self, formatter: Formatter<T>);
	fn write_log(&mut self, log: LogData<T>, flush: bool);
}


/// A file on the filesystem
struct LogFile<T: Eq + Send + Hash + Clone + 'static> {
	file: File,
	path: PathBuf,
	formatter: Formatter<T>
}


impl<T: Eq + Send + Hash + Clone + 'static> LogDump<T> for LogFile<T> {
	fn set_formatter(&mut self, formatter: Formatter<T>) {
		self.formatter = formatter;
	}

	fn write_log(&mut self, log: LogData<T>, flush: bool) {
		self.file.write_all(((self.formatter)(log) + "\n").as_bytes())
			.unwrap_or_else(|_| panic!("Error writing to log file: {:?}", self.path));
		if flush {
			self.file.flush().unwrap_or_else(|_| panic!("Error flushing to log file: {:?}", self.path))
		}

	}
}


/// Uncolored [Stderr] output
///
/// [Stderr]: std::io::Stderr
struct PlainStderr<T: Eq + Send + Hash + Clone + 'static> {
	stderr: Stderr,
	formatter: Formatter<T>
}


impl<T: Eq + Send + Hash + Clone + 'static> LogDump<T> for PlainStderr<T> {
	fn set_formatter(&mut self, formatter: Formatter<T>) {
		self.formatter = formatter;
	}

	fn write_log(&mut self, log: LogData<T>, flush: bool) {
		self.stderr.write_all(((self.formatter)(log) + "\n").as_bytes()).unwrap();
		if flush {
			self.stderr.flush().unwrap()
		}
	}
}


/// Colored [Stderr] output
///
/// Only works with LogLevel
///
/// [Stderr]: std::io::Stderr
struct ColoredStderr {
	stderr: Stderr,
	formatter: Formatter<LogLevel>
}


impl LogDump<LogLevel> for ColoredStderr {
	fn set_formatter(&mut self, formatter: Formatter<LogLevel>) {
		self.formatter = formatter;
	}

	fn write_log(&mut self, log: LogData<LogLevel>, flush: bool) {
		let log_type = log.log_type;
		let msg = (self.formatter)(log) + "\n";
		self.stderr.write_all(
			match log_type {
				LogLevel::Warn => Colorize::yellow(msg.as_str()),
				LogLevel::Error => Colorize::red(msg.as_str()),
				_ => Colorize::white(msg.as_str()),
			}.as_bytes()
		).unwrap();

		if flush {
			self.stderr.flush().unwrap()
		}
	}
}


struct LogDumpProcess<T: Eq + Send + Hash + Clone + 'static> {
	sender: Sender<LogData<T>>,
	paused: bool
}


/// A Logger that facilitates the sending of logs to [LogDump]s.
/// LogDumps are ran in separate synchronous threads.
///
/// When dropped, the Logger will wait on all [LogDump] threads to terminate (which will drop the [LogDump]s)
///
/// [LogDump]: crate::LogDump
pub struct Logger<T: Eq + Send + Hash + Clone + 'static> {
	senders: Mutex<HashMap<usize, LogDumpProcess<T>>>,
	thr_handles: Mutex<Vec<JoinHandle<()>>>,
}


/// Returned by Logger.[log]
///
/// Call [wait] to block until all [LogDump]s have processed the sent log
///
/// [log]: crate::Logger::log
/// [wait]: crate::LogReady::wait
/// [LogDump]: crate::LogDump
pub struct LogReady {
	count: usize,
	receiver: Receiver<()>
}


impl LogReady {
	/// Blocks until all associated [LogDump]s have processed the sent log
	///
	/// [LogDump]: crate::LogDump
	pub fn wait(self) {
		for _ in 0..self.count {
			if self.receiver.recv().is_err() {
				return
			}
		}
	}

	/// Awaits until all logs are processd
	///
	/// The given task will be called repeatedly, with the future awaited on.
	/// This is essentially the polling rate
	///
	/// Usually this would be a sleep task
	pub async fn wait_async<F, Fut>(self, task: F)
		where
			Fut: Future,
			F: Fn() -> Fut
	{
		for _ in 0..self.count {
			loop {
				match self.receiver.try_recv() {
					Err(TryRecvError::Empty) => {
						task().await;
					}
					_ => break,
				}
			}
		}
	}
}


/// A handle to a LogDump thread.
/// To stop that thread from continuing to process logs, close must be called.
/// Dropping this handle detaches the thread, making it impossible to stop the thread,
/// besides closing the logger
pub struct LogDumpHandle<'a, T: Eq + Send + Hash + Clone + 'static> {
	logger_ref: &'a Logger<T>,
	// paused: Arc<AtomicBool>,
	index: usize,
}


impl<'a, T: Eq + Send + Hash + Clone + 'static> LogDumpHandle<'a, T> {
	/// Terminate the associated [LogDump] thread
	///
	/// This will cause the [LogDump] to be dropped
	///
	/// [LogDump]: crate::LogDump
	pub fn close(self) {
		self.logger_ref.detach_log_dump(self.index);
	}
	/// Prevents the associated [LogDump] from accepting logs
	///
	/// It is safe to call this even if the logger is already paused
	///
	/// # Important Note
	/// Dropping the handle after pausing does not unpause the associated [LogDump]
	///
	/// [LogDump]: crate::LogDump
	pub fn pause(&self) {
		self.logger_ref.pause_log_dump(self.index);
	}
	/// Allows the associated [LogDump] to accept logs
	///
	/// It is safe to call this even if the logger was never paused to begin with
	///
	/// [LogDump]: crate::LogDump
	pub fn unpause(&self) {
		self.logger_ref.unpause_log_dump(self.index);
	}
}


impl<T: Eq + Send + Hash + Clone + 'static> Default for Logger<T> {
	fn default() -> Self {
		Self {
			senders: Default::default(),
			thr_handles: Default::default(),
		}
	}
}


impl<T: Eq + Send + Hash + Clone + 'static> Logger<T> {
	/// Create a new Logger with no attached LogDumps.
	/// Equivalent to default
	pub fn new() -> Self {
		Self::default()
	}

	fn detach_log_dump(&self, idx: usize) {
		self.senders.lock().unwrap().remove(&idx);
	}

	fn pause_log_dump(&self, idx: usize) {
		self.senders.lock().unwrap().get_mut(&idx).unwrap().paused = true;
	}

	fn unpause_log_dump(&self, idx: usize) {
		self.senders.lock().unwrap().get_mut(&idx).unwrap().paused = false;
	}

	/// Attach a LogDump to this Logger instance.
	/// filter is a collection of log_types that the LogDump will process. Leave empty to allow all.
	/// Iff always_flush is true, the LogDump will always try to ensure that all data makes it to
	/// the intended output.
	///
	/// A new thread is created every time this function is called
	///
	/// Returned is a [LogDumpHandle] that allows termination of the spawned thread if needed. Dropping it will not do anything and is safe.
	///
	/// [LogDumpHandle]: crate::LogDumpHandle
	pub fn attach_log_dump<D: LogDump<T> + Send + 'static, S: IntoIterator<Item=T>>(&self, mut dump: D, filter: S, always_flush: bool) -> LogDumpHandle<'_, T> {
		let filter = HashSet::<T>::from_iter(filter);
		let (log_sender, log_receiver) = channel();

		let mut senders_lock = self.senders.lock().unwrap();
		let mut index = senders_lock.len();
		while senders_lock.contains_key(&index) {
			index += 1;
		}
		senders_lock.insert(
			index,
			LogDumpProcess {
				sender: log_sender,
				paused: false
			}
		);
		drop(senders_lock);

		let thr = spawn(move || {
			while let Ok(log) = log_receiver.recv() {
				if !filter.is_empty() && !filter.contains(&log.log_type) {
					continue
				}
				let sender = log.log_processed.clone();
				dump.write_log(log, always_flush);
				let _ = sender.send(());
			}
			drop(dump);
		});

		self.thr_handles.lock().unwrap().push(thr);

		LogDumpHandle {
			logger_ref: self,
			index
		}
	}

	/// Attach a Log File. Usage of this method is encouraged over [attach_log_dump] as this will correctly open a log file
	///
	/// [attach_log_dump]: crate::Logger::attach_log_dump
	pub fn attach_log_file<P: AsRef<Path>, S: IntoIterator<Item=T>>(&self, path: P, formatter: Formatter<T>, filter: S, always_flush: bool) -> Result<LogDumpHandle<'_, T>, Error> {
		let path = path.as_ref();
		OpenOptions::new()
			.append(true)
			.create(true)
			.open(path)
			.map(|file| {
				self.attach_log_dump(LogFile { file, path: path.to_path_buf(), formatter }, filter, always_flush)
			})
	}

	/// Attach an output to [stderr] (ie. the console)
	///
	/// [stderr]: std::io::Stderr
	pub fn attach_stderr<S: IntoIterator<Item=T>>(&self, formatter: Formatter<T>, filter: S, always_flush: bool) -> LogDumpHandle<'_, T> {
		self.attach_log_dump(PlainStderr { stderr: stderr(), formatter }, filter, always_flush)
	}

	/// Returns true iff there are [LogDump]s attached
	///
	/// [LogDump]: crate::LogDump
	pub fn has_attachments(&self) -> bool {
		!self.senders.lock().unwrap().is_empty()
	}

	/// Prevent this logger from rerouting logs to [LogDump]s.
	/// All calls to [log] will now panic
	///
	/// [LogDump]: crate::LogDump
	/// [log]: crate::Logger::log
	pub fn close_logger(&self) {
		self.senders.lock().unwrap().clear();
	}

	/// Send a log to all [LogDump]s
	///
	/// Returns a [LogReady] that can be used to wait until the log has been completely processed
	///
	/// # Panic
	/// Will panic if there are no attached log dumps
	///
	/// [LogDump]: crate::LogDump
	/// [LogReady]: crate::LogReady
	pub fn log<M: Display>(&self, message: M, log_type: T, trace_data: Option<(&'static str, u32)>) -> LogReady {
		let lock = self.senders.lock().unwrap();

		assert!(
			!lock.is_empty(),
			"There are no attached log dumps! They may have been explicitly dropped"
		);

		let (sender, receiver) = channel();

		for process in lock.values() {
			if process.paused {
				continue
			}
			process.sender.send(LogData {
				message: message.to_string(),
				time: Local::now(),
				log_type: log_type.clone(),
				trace_data,
				log_processed: sender.clone()
			}).unwrap();
		}

		LogReady {
			count: lock.len(),
			receiver
		}
	}
}


impl Logger<LogLevel> {
	/// Attach an output to [stderr] (ie. the console).
	/// Color will be printed as well
	///
	/// [stderr]: std::io::Stderr
	pub fn attach_colored_stderr<S: IntoIterator<Item=LogLevel>>(&self, formatter: Formatter<LogLevel>, filter: S, always_flush: bool) -> LogDumpHandle<'_, LogLevel> {
		self.attach_log_dump(ColoredStderr { stderr: stderr(), formatter }, filter, always_flush)
	}
}


impl<T: Eq + Send + Hash + Clone + 'static> Drop for Logger<T> {
	fn drop(&mut self) {
		// wait for all threads to exit
		self.close_logger();
		for handle in self.thr_handles.lock().unwrap().drain(..) {
			handle.join().unwrap();
		}
	}
}


impl Logger<LogLevel> {
	/// Send an info log
	pub fn info<T: Display>(&self, msg: T, trace_data: Option<(&'static str, u32)>) {
		self.log(msg, LogLevel::Info, trace_data);
	}
	/// Send an error log
	pub fn error<T: Display>(&self, msg: T, trace_data: Option<(&'static str, u32)>) {
		self.log(msg, LogLevel::Error, trace_data);
	}
	/// Send an debug log
	pub fn debug<T: Display>(&self, msg: T, trace_data: Option<(&'static str, u32)>) {
		self.log(msg, LogLevel::Debug, trace_data);
	}
	/// Send an trace log
	pub fn trace<T: Display>(&self, msg: T, trace_data: Option<(&'static str, u32)>) {
		self.log(msg, LogLevel::Trace, trace_data);
	}
	/// Send an warn log
	pub fn warn<T: Display>(&self, msg: T, trace_data: Option<(&'static str, u32)>) {
		self.log(msg, LogLevel::Warn, trace_data);
	}
}


/// Properly declare a [Logger] instance with the given name and log_type
///
/// declare_logger!(\[pub] \<NAME>) to make public
///
/// [Logger]: crate::Logger
#[macro_export]
macro_rules! declare_logger {
    ($([$vis: tt])? $name: ident) => {
		declare_logger!($([$vis])? $name, $crate::LogLevel);
    };
    ($([$vis: tt])? $name: ident, $log_type: ty) => {
		$crate::lazy_static! {
			$($vis)? static ref $name: $crate::Logger<$log_type> = $crate::Logger::new();
		}
    };
}


macro_rules! define_log_define {
    ($track: ident, $define_name: ident, $name: ident, $(#[$outer:meta])*) => {
		$(#[$outer])*
        #[macro_export]
        macro_rules! $define_name {
            ($logger: path) => {
                $define_name!($logger, $);
            };
            ($logger: path, trace) => {
                $define_name!($logger, trace, $);
            };
            ($logger: path, export) => {
                $define_name!($logger, export, $);
            };
            ($logger: path, trace, export) => {
                $define_name!($logger, trace, export, $);
            };
            ($logger: path, $dol: tt) => {
				/// Log macro without tracing
                macro_rules! $name {
                    ($dol($arg: tt)*) => {
                        $logger.log(format!($dol($arg)*), $crate::LogLevel::$track, None)
                    }
                }
            };
            ($logger: path, trace, $dol: tt) => {
				/// Log macro with tracing
                macro_rules! $name {
                    ($dol($arg: tt)*) => {
                        $logger.log(format!($dol($arg)*), $crate::LogLevel::$track, Some((file!(), line!())))
                    }
                }
            };
            ($logger: path, export, $dol: tt) => {
				/// Log macro without tracing
                #[macro_export]
                macro_rules! $name {
                    ($dol($arg: tt)*) => {
                        $logger.log(format!($dol($arg)*), $crate::LogLevel::$track, None)
                    }
                }
            };
            ($logger: path, trace, export, $dol: tt) => {
				/// Log macro with tracing
                #[macro_export]
                macro_rules! $name {
                    ($dol($arg: tt)*) => {
                        $logger.log(format!($dol($arg)*), $crate::LogLevel::$track, Some((file!(), line!())))
                    }
                }
            };
        }
    }
}


define_log_define!(Info, define_info, info,
/// Defines an info! macro that is bound to a single Logger
///
/// define_info(\<LOGGER>, export) to use macro_export (export must be the last argument)
///
/// define_info(\<LOGGER>, trace) to enable tracing
///
/// Both arguments can also be passed
);
define_log_define!(Warn, define_warn, warn,
/// Defines a warn! macro that is bound to a single Logger
///
/// define_warn(\<LOGGER>, export) to use macro_export (export must be the last argument)
///
/// define_warn(\<LOGGER>, trace) to enable tracing
///
/// Both arguments can also be passed
);
define_log_define!(Error, define_error, error,
/// Defines an error! macro that is bound to a single Logger
///
/// define_error(\<LOGGER>, export) to use macro_export (export must be the last argument)
///
/// define_error(\<LOGGER>, trace) to enable tracing
///
/// Both arguments can also be passed
);
define_log_define!(Debug, define_debug, debug,
/// Defines a debug! macro that is bound to a single Logger
///
/// define_debug(\<LOGGER>, export) to use macro_export (export must be the last argument)
///
/// define_debug(\<LOGGER>, trace) to enable tracing
///
/// Both arguments can also be passed
);


// #[macro_export]
// macro_rules! set {
//     ($($arg: tt),*) => {{
// 		let mut set = std::collections::HashSet::new();
// 		$(
// 			set.insert($arg);
// 		)*
// 		set
// 	}};
// }


pub mod prelude {
	pub use {define_info, define_warn, define_error, define_debug};
	pub use super::declare_logger;
	pub use super::LogLevel;
	pub use super::formatters;
}


#[cfg(test)]
mod tests {
	use std::collections::HashSet;
	use crate::formatters::default_format;
    declare_logger!{[pub] LMAO}
    define_warn!(LMAO);
    define_debug!(LMAO, trace);

    #[test]
    fn it_works() {
		let handle = LMAO.attach_colored_stderr(default_format, HashSet::new(), true);
		warn!("lmao");
		LMAO.attach_colored_stderr(default_format, HashSet::new(), true);
		handle.close();
		warn!("lma3o");
		debug!("waml");
    }
}
