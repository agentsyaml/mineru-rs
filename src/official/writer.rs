use crate::official::OfficialError;

/// Local-only output sink matching the two methods used by upstream ParseResult.save.
pub trait OfficialDataWriter {
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), OfficialError>;
    fn write_string(&mut self, path: &str, text: &str) -> Result<(), OfficialError>;
}
