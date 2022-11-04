use std::fmt::Display;
use std::sync::mpsc::Sender;
use chrono::{DateTime, Local};


pub fn default_format<T: Display + PartialEq + Send + 'static>(log: LogData<T>) -> String {
	if let Some((file, line)) = log.trace_data {
		format!(
			"{} | {} | {}:{} | {}",
			log.time.format("%Y-%m-%d | %H:%M:%S"),
			log.log_type,
			file,
			line,
			log.message
		)
	} else {
		format!(
			"{} | {} | {}",
			log.time.format("%Y-%m-%d | %H:%M:%S"),
			log.log_type,
			log.message
		)
	}
}


#[derive(Clone)]
pub struct LogData<T: PartialEq + Send + 'static> {
	pub log_type: T,
	pub message: String,
	pub time: DateTime<Local>,
	pub trace_data: Option<(&'static str, u32)>,
	pub(crate) log_processed: Sender<()>
}