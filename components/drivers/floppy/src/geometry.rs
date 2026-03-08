pub const SECTORS_PER_TRACK: usize = 18;
pub const SECTOR_SIZE: usize = 512;

#[derive(Debug, Clone, Copy)]
pub struct ChsGeometry {
    pub cylinder: usize,
    pub head: usize,
    pub sector: usize,
}

impl ChsGeometry {
    pub fn from_lba(lba: usize) -> Self {
        let sectors_per_cylinder = 2 * SECTORS_PER_TRACK;
        let cylinder = lba / sectors_per_cylinder;
        let cylinder_offset = lba % sectors_per_cylinder;
        let head = cylinder_offset / SECTORS_PER_TRACK;
        let sector = cylinder_offset % SECTORS_PER_TRACK;

        Self {
            cylinder,
            head,
            sector: sector + 1,
        }
    }
}
