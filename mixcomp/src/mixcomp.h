// mixcomp.h —— mixcomp 公共状态定义（wlroots 0.18）。
// 架构：每个 xdg_toplevel 一个捕获器——不创建输出、不参与场景合成，
// 直接从 client buffer 读纹理，按窗口独立定时器节拍推帧。
// 输入经内建 wayland 客户端走 virtual-pointer/keyboard 协议注入。
#ifndef MIXCOMP_H
#define MIXCOMP_H

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <wayland-client-core.h>
#include <wayland-server-core.h>
#include <wlr/backend.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_keyboard.h>
#include <wlr/types/wlr_pointer.h>
#include <wlr/types/wlr_seat.h>
#include <wlr/types/wlr_text_input_v3.h>
#include <wlr/types/wlr_xdg_shell.h>
#include <wlr/util/log.h>

// H.264 编码器(openh264;HAVE_OPENH264 由 Makefile 依 pkg-config 探测定义)。
#ifdef HAVE_OPENH264
struct h264_encoder;
struct h264_encoder *h264_open(int width, int height, int fps);
void h264_close(struct h264_encoder *enc);
// 编码一帧 BGRA(行距 stride,源尺寸 src_w×src_h,可奇数;编码器内部
// 2 对齐并复制边缘像素)。force_idr 时强制 IDR。返回 Annex-B 数据
// (调用者 free),*out_keyframe = 1 表示 IDR;失败返回 NULL。
unsigned char *h264_encode(struct h264_encoder *enc, const unsigned char *bgra,
		int stride, int src_w, int src_h, int force_idr,
		unsigned long *out_size, int *out_keyframe);
#endif

// 与 mixumd 控制通道的记录类型。
#define REC_CONTROL      0x01
#define REC_VIDEO        0x02
#define REC_VIDEO_H264   0x03
// PNG 帧/补丁（订阅者协商 codec=png）。负载仍是标准 27 字节帧头 +
// （补丁时）20 字节 patch 头 + PNG 数据；agentd 原样转发，宿主按 flags 合成。
#define REC_VIDEO_PNG    0x04

// 视频帧头（与 Go internal/wire 对齐，小端）：
// [0]=0x01 [1:5] windowID [5:9] w [9:13] h [13:17] size [17:19] flags
// [19:21] curX [21:23] curY [23:25] curW [25:27] curH 之后是编码数据。
#define FRAME_HDR 27
#define FRAME_FLAG_KEYFRAME  0x0001
#define FRAME_FLAG_HASCURSOR 0x0002
// bit2=这是 damage 补丁（payload 前 20 字节为 patch 头）。
#define FRAME_FLAG_PATCH     0x0004
// bit3=快照冻结状态帧（无负载，仅 27 字节帧头）。客户端收到后应停止解码，
// 保留最后一帧画面；收到下一个非 frozen 帧即恢复。
#define FRAME_FLAG_FROZEN    0x0008

// Damage 补丁头（小端）：
// [0:4] x [4:8] y [8:12] w [12:16] h [16:20] format
// format=0 表示仅光标更新（无图像数据）；format=2 表示 PNG。
#define PATCH_HDR 20
#define PATCH_FORMAT_NONE 0
#define PATCH_FORMAT_PNG  2

struct mixcomp_server;

// clipboard.c 的内部类型（服务器持有指针，定义在 clipboard.c）。
struct clip_source;
struct clip_read;

// window —— 一个被隔离流送的顶层窗口。
struct mixcomp_window {
	struct mixcomp_server *server;
	uint32_t id;
	struct wlr_xdg_surface *xdg;
	struct wlr_surface *surface;
	// 客户端 pid（window.open 里带给 agentd，用于 GUI 会话匹配）。
	int pid;

	char app_id[128];
	char title[256];
	int width, height;

	// 流送状态。
	bool want_stream;
	int fps;
	int cursor_x, cursor_y;
	bool have_cursor;
	// H.264 编码器(订阅者协商 h264 且 openh264 可用时创建;NULL = JPEG)
	struct h264_encoder *h264;
	int h264_w, h264_h;        // 编码器对应的源尺寸(重建判据)
	bool h264_idr_pending;

	// PNG + damage 流送（stream.start 的 codec="png" 时启用）。
	bool png_mode;
	int jpeg_quality;          // 每窗口 JPEG 质量（stream.start 可覆盖）
	uint8_t *prev;             // 上一帧完整 RGBA，用于差分
	size_t prev_cap;
	int prev_w, prev_h;
	uint32_t prev_fmt;
	bool have_prev;
	int last_cursor_x, last_cursor_y;
	uint16_t last_cursor_w, last_cursor_h;
	bool last_have_cursor;

	struct wl_event_source *timer; // 帧节拍
	struct wl_listener map;
	struct wl_listener unmap;
	struct wl_listener commit;
	struct wl_listener destroy;
	struct wl_listener set_title;
	struct wl_listener set_app_id;

	struct wl_list link;
};

struct mixcomp_server {
	struct wl_display *display;
	struct wlr_backend *backend;
	struct wlr_renderer *renderer;

	struct wlr_xdg_shell *xdg_shell;
	struct wlr_seat *seat;

	// 内建 wayland 客户端（virtual pointer/keyboard 的注入通道）。
	struct wl_display *inj_display;
	struct wl_event_source *inj_src;
	// 虚拟设备管理器就绪（new_virtual_*）与注入事件路由（见 main.c）。
	struct wl_listener new_input;
	struct wl_listener new_keyboard;
	struct wl_listener inj_motion;
	struct wl_listener inj_button;
	struct wl_listener inj_axis;
	struct wl_listener inj_key;
	struct wl_listener inj_mods;
	// 注入目标（input.c 设置，路由回调读取）。
	struct mixcomp_window *focus_window;
	int inj_width, inj_height;

	struct wl_listener new_surface;
	struct wl_list windows; // mixcomp_window::link
	uint32_t next_window_id;

	// 到 mixumd 的控制通道。
	int sock_fd;
	char sock_path[256];
	bool sock_ready;
	// 接收累积缓冲：半条记录留到下一轮处理，EAGAIN 不再误判连接死亡。
	uint8_t *rx_buf;
	size_t rx_cap;
	size_t rx_have;

	// 快照冻结：暂停全部窗口的捕获节拍（snapshot.freeze / snapshot.thaw）。
	bool snapshot_frozen;

	int jpeg_quality;

	// 剪贴板同步（clipboard.c）：内建 selection source + 选区读回。
	struct clip_source *clip_source;
	struct clip_read *clip_read;
	struct wl_listener clip_sel_req;  // seat request_set_selection
	struct wl_listener clip_sel_set;  // seat set_selection（外部替换自清理）

	// 文本提交（text_input.c）：客户端 text-input-v3 对象与焦点窗口。
	struct wlr_text_input_manager_v3 *text_input_mgr;
	struct wl_list text_inputs; // struct mixcomp_text_input::link
	struct wl_listener new_text_input;
};

// window.c
void window_create(struct mixcomp_server *s, struct wlr_xdg_surface *xdg);
// 快照冻结联动：暂停/恢复全部窗口的捕获定时器。
void windows_set_capture_paused(struct mixcomp_server *s, bool paused);

// stream.c —— 控制通道与帧发送。
bool stream_connect(struct mixcomp_server *s);
void stream_flush(struct mixcomp_server *s);
void stream_send_window(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *type);
void stream_send_frame(struct mixcomp_server *s, struct mixcomp_window *w,
		const unsigned char *rgba, int stride, int width, int height,
		uint32_t drm_format);
void stream_poll_control(struct mixcomp_server *s);
// 上行容器剪贴板文本（JSON 控制消息，type=clipboard）。
void stream_send_clipboard(struct mixcomp_server *s, uint32_t window_id,
		const char *text);
// 控制通道就绪后上报 comp 能力（剪贴板握手）。
void stream_send_capabilities(struct mixcomp_server *s);

// clipboard.c —— 剪贴板同步（console ↔ 容器 Wayland 选区）。
void clipboard_init(struct mixcomp_server *s);
void clipboard_set(struct mixcomp_server *s, const char *text);

// input.c —— 内建注入客户端。
bool input_init(struct mixcomp_server *s);
void input_handle_json(struct mixcomp_server *s, const char *json, size_t len);

// text_input.c —— 宿主输入法提交的文本注入焦点窗口。
void text_input_init(struct mixcomp_server *s);
void text_input_focus(struct mixcomp_server *s, struct mixcomp_window *w);
void text_input_commit(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *text);
void text_input_forget_window(struct mixcomp_server *s,
		struct mixcomp_window *w);

#endif
