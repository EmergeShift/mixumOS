//! 二进制帧:通道标签 + video 帧头。
//! 布局与已移除的 Go 参考实现逐字节一致(小端),由 tests/cross-protocol 锁定。

/// 二进制帧首字节的通道标签。文本帧是 envelope JSON,二者互斥。
pub mod chan {
    pub const VIDEO: u8 = 0x01;
    pub const AUDIO: u8 = 0x02; // 预留
    pub const CHUNK: u8 = 0x03; // 快照块传输
    pub const STDIO: u8 = 0x04; // 任务 stdio(预留)
    pub const FILE: u8 = 0x05; // 文件传输
}

/// 文件帧头:通道标签(1) + transfer id(4) + offset(8)。
pub const FILE_HEADER_SIZE: usize = 13;

/// 打包文件块。`transfer` 是 `file.put` / `file.get` 返回的 id。
pub fn marshal_file(transfer: u32, offset: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FILE_HEADER_SIZE + data.len());
    out.push(chan::FILE);
    out.extend_from_slice(&transfer.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// 解包文件块。
pub fn unmarshal_file(buf: &[u8]) -> Result<(u32, u64, &[u8]), TruncatedFrame> {
    if buf.len() < FILE_HEADER_SIZE || buf[0] != chan::FILE {
        return Err(TruncatedFrame);
    }
    let transfer = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
    let offset = u64::from_le_bytes([
        buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11], buf[12],
    ]);
    Ok((transfer, offset, &buf[FILE_HEADER_SIZE..]))
}

/// video 帧头总长(含通道标签,即数据区起始偏移)。
pub const VIDEO_HEADER_SIZE: usize = 27;

pub const FRAME_FLAG_KEYFRAME: u16 = 1 << 0;
pub const FRAME_FLAG_HAS_CURSOR: u16 = 1 << 1;
/// 这是 damage 补丁：27 字节帧头后先跟 20 字节 patch 头，再是编码数据。
pub const FRAME_FLAG_PATCH: u16 = 1 << 2;
/// patch 头：x/y/w/h/format 各 u32（小端）。
pub const PATCH_HEADER_SIZE: usize = 20;
pub const PATCH_FORMAT_NONE: u32 = 0;
pub const PATCH_FORMAT_PNG: u32 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame<'a> {
    pub window_id: u32,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    pub has_cursor: bool,
    pub cursor_x: i16,
    pub cursor_y: i16,
    pub cursor_w: u16,
    pub cursor_h: u16,
    pub data: &'a [u8],
}

#[derive(Debug, thiserror::Error)]
#[error("binary frame truncated")]
pub struct TruncatedFrame;

/// 打包一帧视频(含通道标签,总长 = 1 + 27 + data.len())。
pub fn marshal_video(f: &VideoFrame, out: &mut Vec<u8>) {
    let mut flags = 0u16;
    if f.keyframe {
        flags |= FRAME_FLAG_KEYFRAME;
    }
    if f.has_cursor {
        flags |= FRAME_FLAG_HAS_CURSOR;
    }
    out.clear();
    out.reserve(1 + VIDEO_HEADER_SIZE + f.data.len());
    out.push(chan::VIDEO);
    out.extend_from_slice(&f.window_id.to_le_bytes());
    out.extend_from_slice(&f.width.to_le_bytes());
    out.extend_from_slice(&f.height.to_le_bytes());
    out.extend_from_slice(&(f.data.len() as u32).to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(f.cursor_x as u16).to_le_bytes());
    out.extend_from_slice(&(f.cursor_y as u16).to_le_bytes());
    out.extend_from_slice(&f.cursor_w.to_le_bytes());
    out.extend_from_slice(&f.cursor_h.to_le_bytes());
    out.extend_from_slice(f.data);
}

/// 解包一帧视频。buf 从通道标签开始。
pub fn unmarshal_video(buf: &[u8]) -> Result<VideoFrame<'_>, TruncatedFrame> {
    // 布局总长含通道标签:1 + 26 字段 = 27,数据从下标 27 开始。
    if buf.len() < VIDEO_HEADER_SIZE || buf[0] != chan::VIDEO {
        return Err(TruncatedFrame);
    }
    let b = &buf[1..];
    let rd_u32 = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let rd_u16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let flags = rd_u16(16);
    let size = rd_u32(12) as usize;
    let data_start = VIDEO_HEADER_SIZE;
    if size > buf.len() - data_start {
        return Err(TruncatedFrame);
    }
    Ok(VideoFrame {
        window_id: rd_u32(0),
        width: rd_u32(4),
        height: rd_u32(8),
        keyframe: flags & FRAME_FLAG_KEYFRAME != 0,
        has_cursor: flags & FRAME_FLAG_HAS_CURSOR != 0,
        cursor_x: rd_u16(18) as i16,
        cursor_y: rd_u16(20) as i16,
        cursor_w: rd_u16(22),
        cursor_h: rd_u16(24),
        data: &buf[data_start..data_start + size],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_round_trip() {
        let frame = VideoFrame {
            window_id: 42,
            width: 1920,
            height: 1080,
            keyframe: true,
            has_cursor: true,
            cursor_x: -3,
            cursor_y: 700,
            cursor_w: 24,
            cursor_h: 24,
            data: b"jpegdata\xe6\xb5\x8b\xe8\xaf\x95",
        };
        let mut buf = Vec::new();
        marshal_video(&frame, &mut buf);
        let got = unmarshal_video(&buf).expect("解包应成功");
        assert_eq!(got.window_id, 42);
        assert_eq!(got.width, 1920);
        assert_eq!(got.height, 1080);
        assert!(got.keyframe);
        assert!(got.has_cursor);
        assert_eq!(got.cursor_x, -3);
        assert_eq!(got.cursor_y, 700);
        assert_eq!(got.data, frame.data);
    }

    #[test]
    fn layout_matches_reference_bytes() {
        // 与 Go MarshalVideo 的第一个测试帧逐字节对齐:
        // windowID=42, 1920x1080, keyframe+cursor, cur=(-3,700,24,24), data="hi"
        let frame = VideoFrame {
            window_id: 42,
            width: 1920,
            height: 1080,
            keyframe: true,
            has_cursor: true,
            cursor_x: -3,
            cursor_y: 700,
            cursor_w: 24,
            cursor_h: 24,
            data: b"hi",
        };
        let mut buf = Vec::new();
        marshal_video(&frame, &mut buf);
        let expect: Vec<u8> = [
            &[0x01u8][..],
            &42u32.to_le_bytes(),
            &1920u32.to_le_bytes(),
            &1080u32.to_le_bytes(),
            &2u32.to_le_bytes(),
            &3u16.to_le_bytes(), // KEYFRAME|HAS_CURSOR
            &(-3i16 as u16).to_le_bytes(),
            &700u16.to_le_bytes(),
            &24u16.to_le_bytes(),
            &24u16.to_le_bytes(),
            b"hi",
        ]
        .concat();
        assert_eq!(buf, expect);
    }

    #[test]
    fn file_round_trip() {
        let frame = marshal_file(7, 123, b"payload");
        let (transfer, offset, data) = unmarshal_file(&frame).expect("file frame");
        assert_eq!(transfer, 7);
        assert_eq!(offset, 123);
        assert_eq!(data, b"payload");
    }

    #[test]
    fn rejects_truncated() {
        assert!(unmarshal_video(&[chan::VIDEO, 0, 0]).is_err());
    }
}
