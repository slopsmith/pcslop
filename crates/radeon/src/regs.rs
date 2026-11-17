//! Register offsets and bits, named as in xf86-video-ati's radeon_reg.h
//! (MIT license; ATI Technologies Inc. and VA Linux Systems Inc.).

pub const MM_INDEX: u32 = 0x0000;
pub const MM_DATA: u32 = 0x0004;
pub const CLOCK_CNTL_INDEX: u32 = 0x0008;
pub const CLOCK_CNTL_DATA: u32 = 0x000c;
pub const PLL_WR_EN: u32 = 1 << 7;
pub const DAC_CNTL: u32 = 0x0058;
pub const DAC_8BIT_EN: u32 = 1 << 8;
pub const CONFIG_APER_0_BASE: u32 = 0x0100;
pub const CONFIG_APER_1_BASE: u32 = 0x0104;
pub const CONFIG_APER_SIZE: u32 = 0x0108;
pub const CONFIG_REG_1_BASE: u32 = 0x010c;
pub const CONFIG_REG_APER_SIZE: u32 = 0x0110;
pub const RBBM_STATUS: u32 = 0x0e40;
/// RBBM_STATUS with all 64 FIFO slots free and every engine idle.
pub const RBBM_FIFOCNT_IDLE: u32 = 0x40;
