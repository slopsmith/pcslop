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
pub const MEM_VGA_WP_SEL: u32 = 0x0038;
pub const MEM_VGA_RP_SEL: u32 = 0x003c;
pub const GEN_INT_CNTL: u32 = 0x0040;
pub const GEN_INT_STATUS: u32 = 0x0044;
pub const VSYNC_INT: u32 = 1 << 2;
pub const CRTC_GEN_CNTL: u32 = 0x0050;
pub const CRTC_DBL_SCAN_EN: u32 = 1 << 0;
pub const CRTC_CUR_EN: u32 = 1 << 16;
pub const CRTC_EXT_DISP_EN: u32 = 1 << 24;
pub const CRTC_EN: u32 = 1 << 25;
pub const CRTC_EXT_CNTL: u32 = 0x0054;
pub const VGA_ATI_LINEAR: u32 = 1 << 3;
pub const VGA_MEM_PS_EN: u32 = 1 << 19;
pub const CRTC_DISPLAY_DIS: u32 = 1 << 10;
pub const CRTC_STATUS: u32 = 0x005c;
pub const CRTC_VBLANK_CUR: u32 = 1 << 0;
pub const CRTC_VBLANK_SAVE: u32 = 1 << 1;
pub const PALETTE_INDEX: u32 = 0x00b0;
pub const PALETTE_DATA: u32 = 0x00b4;
pub const PALETTE_30_DATA: u32 = 0x00b8;
pub const CRTC_H_TOTAL_DISP: u32 = 0x0200;
pub const CRTC_V_TOTAL_DISP: u32 = 0x0208;
pub const CRTC_VLINE_CRNT_VLINE: u32 = 0x0210;
pub const CRTC_CRNT_FRAME: u32 = 0x0214;
pub const CRTC_OFFSET: u32 = 0x0224;
pub const CRTC_PITCH: u32 = 0x022c;
pub const CUR_OFFSET: u32 = 0x0260;
pub const CUR_HORZ_VERT_POSN: u32 = 0x0264;
pub const CUR_HORZ_VERT_OFF: u32 = 0x0268;
pub const DAC_CMP_EN: u32 = 1 << 3;
pub const DAC_CMP_OUTPUT: u32 = 1 << 7;
pub const GPIO_VGA_DDC: u32 = 0x0060;
pub const GPIO_DVI_DDC: u32 = 0x0064;
pub const GPIO_MONID: u32 = 0x0068;
pub const GPIO_CRT2_DDC: u32 = 0x006c;
pub const GPIO_A_0: u32 = 1 << 0;
pub const GPIO_A_1: u32 = 1 << 1;
pub const GPIO_Y_0: u32 = 1 << 8;
pub const GPIO_Y_1: u32 = 1 << 9;
pub const GPIO_EN_0: u32 = 1 << 16;
pub const GPIO_EN_1: u32 = 1 << 17;
pub const MC_STATUS: u32 = 0x0150;
/// MC_STATUS bits the memory controller always reports: memory power-up
/// complete on all four channels (R300_MEM_PWRUP_COMPLETE, which XP's ATI
/// driver waits for after an SDRAM reset), MC_IDLE for R100 to R200 and
/// R300_MC_IDLE from R300 on.
pub const MC_STATUS_IDLE: u32 = 0x0f | 1 << 2 | 1 << 4;
pub const MC_FB_LOCATION: u32 = 0x0148;
pub const DISPLAY_BASE_ADDR: u32 = 0x023c;
pub const PPLL_REF_DIV: usize = 0x03;
pub const P2PLL_REF_DIV: usize = 0x2b;
/// PPLL_REF_DIV and P2PLL_REF_DIV: ATOMIC_UPDATE_W on writes, ATOMIC_UPDATE_R on reads.
pub const PLL_ATOMIC_UPDATE: u32 = 1 << 15;
pub const BUS_CNTL: u32 = 0x0030;
pub const RBBM_SOFT_RESET: u32 = 0x00f0;
pub const SOFT_RESET_CP: u32 = 1 << 0;
pub const SOFT_RESET_E2: u32 = 1 << 5;
pub const MC_AGP_LOCATION: u32 = 0x014c;
pub const AGP_BASE: u32 = 0x0170;
pub const AIC_CNTL: u32 = 0x01d0;
pub const PCIGART_TRANSLATE_EN: u32 = 1 << 0;
pub const AIC_PT_BASE: u32 = 0x01d8;
pub const AIC_LO_ADDR: u32 = 0x01dc;
pub const AIC_HI_ADDR: u32 = 0x01e0;
pub const CRTC_OFFSET_CNTL: u32 = 0x0228;
pub const R300_CRTC_X_Y_MODE_EN: u32 = 1 << 9;
pub const R300_CRTC_TILE_X0_Y0: u32 = 0x0350;

pub const CRTC_VBLANK_STAT: u32 = 1 << 0;
pub const CRTC2_VBLANK_STAT: u32 = 1 << 9;
pub const SW_INT_TEST: u32 = 1 << 25;
pub const SW_INT_FIRE: u32 = 1 << 26;

pub const CP_RB_BASE: u32 = 0x0700;
pub const CP_RB_CNTL: u32 = 0x0704;
pub const RB_NO_UPDATE: u32 = 1 << 27;
pub const RB_RPTR_WR_ENA: u32 = 1 << 31;
pub const CP_RB_RPTR_ADDR: u32 = 0x070c;
pub const CP_RB_RPTR: u32 = 0x0710;
pub const CP_RB_WPTR: u32 = 0x0714;
pub const CP_RB_RPTR_WR: u32 = 0x071c;
pub const CP_IB_BASE: u32 = 0x0738;
pub const CP_IB_BUFSZ: u32 = 0x073c;
pub const CP_CSQ_CNTL: u32 = 0x0740;
pub const CP_CSQ_MODE: u32 = 0x0744;
pub const CSQ_PRIMARY_BM: u32 = 1 << 30;
pub const CSQ_PRIMARY_ENABLE: u32 = 1 << 31;
pub const SCRATCH_UMSK: u32 = 0x0770;
pub const CP_RESYNC_ADDR: u32 = 0x0778;
pub const CP_RESYNC_DATA: u32 = 0x077c;
pub const SCRATCH_ADDR: u32 = 0x0774;
pub const CP_STAT: u32 = 0x07c0;
pub const CP_CSQ_STAT: u32 = 0x07f8;
pub const SCRATCH_REG0: u32 = 0x15e0;
pub const SCRATCH_REG7: u32 = 0x15fc;

pub const BRUSH_DATA0: u32 = 0x1480;
pub const BRUSH_DATA63: u32 = 0x157c;
pub const BRUSH_Y_X: u32 = 0x1474;
pub const DP_BRUSH_BKGD_CLR: u32 = 0x1478;
pub const DP_BRUSH_FRGD_CLR: u32 = 0x147c;
pub const DP_GUI_MASTER_CNTL: u32 = 0x146c;
pub const GMC_SRC_PITCH_OFFSET_CNTL: u32 = 1 << 0;
pub const GMC_DST_PITCH_OFFSET_CNTL: u32 = 1 << 1;
pub const GMC_SRC_CLIPPING: u32 = 1 << 2;
pub const GMC_DST_CLIPPING: u32 = 1 << 3;
pub const GMC_CLR_CMP_CNTL_DIS: u32 = 1 << 28;
pub const GMC_WR_MSK_DIS: u32 = 1 << 30;
pub const GMC_LD_BRUSH_Y_X: u32 = 1 << 31;
pub const DP_CNTL: u32 = 0x16c0;
pub const DST_X_LEFT_TO_RIGHT: u32 = 1 << 0;
pub const DST_Y_TOP_TO_BOTTOM: u32 = 1 << 1;
pub const DP_DATATYPE: u32 = 0x16c4;
pub const DP_MIX: u32 = 0x16c8;
pub const DP_WRITE_MASK: u32 = 0x16cc;
pub const DP_SRC_FRGD_CLR: u32 = 0x15d8;
pub const DP_SRC_BKGD_CLR: u32 = 0x15dc;
pub const CLR_CMP_CNTL: u32 = 0x15c0;
pub const CLR_CMP_CLR_SRC: u32 = 0x15c4;
pub const CLR_CMP_CLR_DST: u32 = 0x15c8;
pub const CLR_CMP_MASK: u32 = 0x15cc;
pub const DST_OFFSET: u32 = 0x1404;
pub const DST_PITCH: u32 = 0x1408;
pub const DST_WIDTH: u32 = 0x140c;
pub const DST_HEIGHT: u32 = 0x1410;
pub const SRC_X: u32 = 0x1414;
pub const SRC_Y: u32 = 0x1418;
pub const DST_X: u32 = 0x141c;
pub const DST_Y: u32 = 0x1420;
pub const SRC_PITCH_OFFSET: u32 = 0x1428;
pub const DST_PITCH_OFFSET: u32 = 0x142c;
pub const SRC_Y_X: u32 = 0x1434;
pub const DST_Y_X: u32 = 0x1438;
pub const DST_HEIGHT_WIDTH: u32 = 0x143c;
pub const DST_HEIGHT_WIDTH_8: u32 = 0x158c;
pub const SRC_X_Y: u32 = 0x1590;
pub const DST_X_Y: u32 = 0x1594;
pub const DST_WIDTH_HEIGHT: u32 = 0x1598;
pub const SRC_OFFSET: u32 = 0x15ac;
pub const SRC_PITCH: u32 = 0x15b0;
pub const DST_LINE_START: u32 = 0x1600;
pub const DST_LINE_END: u32 = 0x1604;
pub const DST_LINE_PATCOUNT: u32 = 0x1608;
pub const SC_LEFT: u32 = 0x1640;
pub const SC_RIGHT: u32 = 0x1644;
pub const SC_TOP: u32 = 0x1648;
pub const SC_BOTTOM: u32 = 0x164c;
pub const SRC_SC_RIGHT: u32 = 0x1654;
pub const SRC_SC_BOTTOM: u32 = 0x165c;
pub const DEFAULT_OFFSET: u32 = 0x16e0;
pub const DEFAULT_PITCH: u32 = 0x16e4;
pub const DEFAULT_SC_BOTTOM_RIGHT: u32 = 0x16e8;
pub const SC_TOP_LEFT: u32 = 0x16ec;
pub const SC_BOTTOM_RIGHT: u32 = 0x16f0;
pub const SRC_SC_BOTTOM_RIGHT: u32 = 0x16f4;
pub const DST_PITCH_OFFSET_C: u32 = 0x1c80;
pub const DP_GUI_MASTER_CNTL_C: u32 = 0x1c84;
pub const SC_TOP_LEFT_C: u32 = 0x1c88;
pub const SC_BOTTOM_RIGHT_C: u32 = 0x1c8c;
pub const HOST_DATA0: u32 = 0x17c0;
pub const HOST_DATA_LAST: u32 = 0x17e0;
/// An undocumented HOST_DATA port after HOST_DATA_LAST. XP's ATI driver
/// streams texture uploads through it.
pub const HOST_DATA_PORT: u32 = 0x17e4;
pub const RB2D_DSTCACHE_CTLSTAT: u32 = 0x342c;
pub const DSTCACHE_CTLSTAT: u32 = 0x1714;
pub const RB3D_DSTCACHE_CTLSTAT: u32 = 0x325c;
pub const R300_RB3D_DSTCACHE_CTLSTAT: u32 = 0x4e4c;
pub const R300_RB3D_ZCACHE_CTLSTAT: u32 = 0x4f18;
/// RB2D_DC_BUSY and its 3D equivalents.
pub const CACHE_BUSY: u32 = 1 << 31;
pub const I2C_CNTL_0: u32 = 0x0090;
pub const I2C_DONE: u32 = 1 << 0;
pub const I2C_NACK: u32 = 1 << 1;
pub const I2C_HALT: u32 = 1 << 2;
pub const I2C_SOFT_RST: u32 = 1 << 5;
pub const I2C_START: u32 = 1 << 8;
pub const I2C_STOP: u32 = 1 << 9;
pub const I2C_RECEIVE: u32 = 1 << 10;
pub const I2C_ABORT: u32 = 1 << 11;
pub const I2C_GO: u32 = 1 << 12;
pub const I2C_CNTL_1: u32 = 0x0094;
pub const I2C_DATA: u32 = 0x0098;
/// Undocumented queue status registers that XP's ATI driver polls until the
/// engine is idle: the low five bits of 0x790 read 2 and those of 0x7B0 read 8,
/// then 0xA10's low half reads 0x3333.
/// The IDCT and motion compensation engine (MPEG decode), from 0x1F80 to
/// 0x1FBC. radeon_reg.h names only RUNS, LEVELS, AUTH_CONTROL, AUTH and
/// CONTROL. XP's ATI driver polls 0x1FB4 until bit 31 is set before it
/// changes the display mode, so we report the engine as idle there.
pub const IDCT_STATUS: u32 = 0x1fb4;
pub const IDCT_IDLE: u32 = 1 << 31;
pub const IDLE_QUEUE_STATUS: [(u32, u32); 3] = [(0x0790, 0x02), (0x07b0, 0x08), (0x0a10, 0x3333)];
