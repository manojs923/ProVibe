#[derive(Debug, Clone)]
pub struct AudioSegment {
    pub data: Vec<u8>,
}

pub struct AudioTap;

impl AudioTap {
    pub fn new(_enabled: bool) -> Self { Self }
    pub fn drain(&mut self) -> Vec<AudioSegment> { Vec::new() }
}