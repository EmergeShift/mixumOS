// main.c —— mixcomp 启动：headless 后端、xdg_shell、seat、
// 虚拟输入管理器、注入客户端、控制通道轮询。
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/timerfd.h>
#include <unistd.h>

#include <wlr/backend.h>
#include <wlr/backend/headless.h>
#include <wlr/backend/multi.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_output.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_data_device.h>
#include <wlr/types/wlr_shm.h>
#include <wlr/types/wlr_subcompositor.h>
#include <wlr/types/wlr_viewporter.h>
#include <wlr/types/wlr_virtual_keyboard_v1.h>
#include <wlr/types/wlr_virtual_pointer_v1.h>
#include <wlr/types/wlr_xdg_shell.h>

#include "mixcomp.h"

static struct mixcomp_server server = {0};

// ---------- 注入事件 → seat 路由 ----------

static void route_motion_absolute(struct wl_listener *l, void *data) {
	struct wlr_pointer_motion_absolute_event *ev = data;
	struct mixcomp_server *s = wl_container_of(l, s, inj_motion);
	struct mixcomp_window *w = s->focus_window;
	if (w == NULL || w->surface == NULL) {
		return;
	}
	double x = (double)ev->x, y = (double)ev->y;
	if (s->seat->pointer_state.focused_surface != w->surface) {
		wlr_seat_pointer_notify_enter(s->seat, w->surface, x, y);
	}
	wlr_seat_pointer_notify_motion(s->seat, ev->time_msec, x, y);
	w->cursor_x = (int)x;
	w->cursor_y = (int)y;
	w->have_cursor = true;
}

static void route_button(struct wl_listener *l, void *data) {
	struct wlr_pointer_button_event *ev = data;
	struct mixcomp_server *s = wl_container_of(l, s, inj_button);
	if (s->focus_window == NULL) {
		return;
	}
	wlr_seat_pointer_notify_button(s->seat, ev->time_msec, ev->button, ev->state);
}

static void route_axis(struct wl_listener *l, void *data) {
	struct wlr_pointer_axis_event *ev = data;
	struct mixcomp_server *s = wl_container_of(l, s, inj_axis);
	if (s->focus_window == NULL) {
		return;
	}
	wlr_seat_pointer_notify_axis(s->seat, ev->time_msec, ev->orientation,
			ev->delta, ev->delta_discrete, ev->source, ev->relative_direction);
}

static void route_key(struct wl_listener *l, void *data) {
	struct wlr_keyboard_key_event *ev = data;
	struct mixcomp_server *s = wl_container_of(l, s, inj_key);
	if (s->focus_window == NULL) {
		return;
	}
	wlr_seat_keyboard_notify_key(s->seat, ev->time_msec, ev->keycode, ev->state);
}

static void route_modifiers(struct wl_listener *l, void *data) {
	(void)data; // modifiers 信号不带事件负载，状态在键盘对象上
	struct mixcomp_server *s = wl_container_of(l, s, inj_mods);
	struct wlr_keyboard *kb = wlr_seat_get_keyboard(s->seat);
	if (kb == NULL) {
		return;
	}
	wlr_seat_keyboard_notify_modifiers(s->seat, &kb->modifiers);
}

// new_input_cb —— 虚拟设备就绪：接 seat、挂事件路由。
static void new_input_cb(struct wl_listener *listener, void *data) {
	(void)listener;
	struct wlr_input_device *dev = data;
	switch (dev->type) {
	case WLR_INPUT_DEVICE_KEYBOARD: {
		struct wlr_keyboard *kb = wlr_keyboard_from_input_device(dev);
		wlr_seat_set_keyboard(server.seat, kb);
		if (server.inj_key.notify == NULL) {
			server.inj_key.notify = route_key;
			wl_signal_add(&kb->events.key, &server.inj_key);
			server.inj_mods.notify = route_modifiers;
			wl_signal_add(&kb->events.modifiers, &server.inj_mods);
		}
		break;
	}
	case WLR_INPUT_DEVICE_POINTER: {
		struct wlr_pointer *ptr = wlr_pointer_from_input_device(dev);
		if (server.inj_motion.notify == NULL) {
			server.inj_motion.notify = route_motion_absolute;
			wl_signal_add(&ptr->events.motion_absolute, &server.inj_motion);
			server.inj_button.notify = route_button;
			wl_signal_add(&ptr->events.button, &server.inj_button);
			server.inj_axis.notify = route_axis;
			wl_signal_add(&ptr->events.axis, &server.inj_axis);
		}
		break;
	}
	default:
		break;
	}
}

// new_toplevel_cb —— xdg toplevel 就绪。wlroots 0.18 语义：new_surface 只在
// get_xdg_surface 时发射一次且 role 必为 NONE，不能做窗口入口；带 role 的
// 事件是 new_toplevel（tinywl 0.18 同款）。
static void new_toplevel_cb(struct wl_listener *listener, void *data) {
	(void)listener;
	struct wlr_xdg_toplevel *toplevel = data;
	// 常规合成器在此摆放窗口；mixcomp 只做隔离捕获。
	window_create(&server, toplevel->base);
}

// 控制通道轮询定时器（200ms）。
static int poll_timerfd = -1;
static int poll_timer_cb(int fd, uint32_t mask, void *data) {
	(void)mask; (void)data;
	stream_poll_control(&server);
	uint64_t expirations;
	read(fd, &expirations, sizeof(expirations));
	stream_flush(&server);
	return 0;
}

int main(int argc, char **argv) {
	// 0.18 的 wlr_log_init 不再读环境变量；保留 WLR_LOG_LEVEL 覆盖能力，
	// e2e 排障需要 DEBUG 级日志（如缓冲类型判定失败时的上下文）。
	const char *log_lvl = getenv("WLR_LOG_LEVEL");
	wlr_log_init(log_lvl != NULL ? (enum wlr_log_importance)atoi(log_lvl) : WLR_INFO, NULL);

	const char *sock = "/run/mixum/display.sock";
	int quality = 80;
	for (int i = 1; i < argc; i++) {
		if (strcmp(argv[i], "--sock") == 0 && i + 1 < argc) {
			sock = argv[++i];
		} else if (strcmp(argv[i], "--quality") == 0 && i + 1 < argc) {
			quality = atoi(argv[++i]);
		}
	}

	// 容器内 headless；无 GPU 时 pixman 渲染（CPU 捕获路径）。
	setenv("WLR_RENDERER", "pixman", 0);

	server.jpeg_quality = quality;
	server.sock_fd = -1;
	snprintf(server.sock_path, sizeof(server.sock_path), "%s", sock);
	wl_list_init(&server.windows);

	server.display = wl_display_create();
	if (server.display == NULL) {
		wlr_log(WLR_ERROR, "wl_display_create 失败");
		return 1;
	}
	struct wl_event_loop *loop = wl_display_get_event_loop(server.display);

	// multi + headless 子后端：virtual-pointer/keyboard 的设备事件经 multi
	// 转发（与 sway 同构），anchor 输出建在 headless 子后端上。
	server.backend = wlr_multi_backend_create(loop);
	struct wlr_backend *headless = wlr_headless_backend_create(loop);
	if (server.backend == NULL || headless == NULL ||
			!wlr_multi_backend_add(server.backend, headless)) {
		wlr_log(WLR_ERROR, "headless 后端创建失败");
		return 1;
	}
	server.new_input.notify = new_input_cb;
	wl_signal_add(&server.backend->events.new_input, &server.new_input);
	server.renderer = wlr_renderer_autocreate(server.backend);
	if (server.renderer == NULL) {
		wlr_log(WLR_ERROR, "渲染器创建失败");
		return 1;
	}
	// wlroots 0.18 已移除 wl_display_init_shm：若误调用会解析到
	// libwayland-server 的遗留实现，其缓冲 wlroots 无法识别，客户端
	// 一贴帧即被"Unknown buffer type"断连。必须用 wlroots 自己的 API。
	if (wlr_shm_create_with_renderer(server.display, 2, server.renderer) == NULL) {
		wlr_log(WLR_ERROR, "wl_shm 创建失败");
		return 1;
	}
	// wl_compositor 是客户端连接的前提（缺它应用会报 no compositor）。
	wlr_compositor_create(server.display, 6, server.renderer);
	wlr_subcompositor_create(server.display);
	wlr_viewporter_create(server.display);

	server.xdg_shell = wlr_xdg_shell_create(server.display, 3);
	server.new_surface.notify = new_toplevel_cb;
	wl_signal_add(&server.xdg_shell->events.new_toplevel, &server.new_surface);

	// 常驻虚拟输出：为要求至少一个显示器的客户端（如终端）提供锚点。
	// 捕获不经过它——每窗口直接读 client buffer。
	// 注意：wlroots 0.18 起不再自动创建 wl_output 全局，合成器必须显式
	// 调 wlr_output_create_global，否则客户端看不到任何显示器（foot 报
	// "no monitors available" 即此因）。
	struct wlr_output *anchor = wlr_headless_add_output(headless, 1280, 800);
	if (anchor != NULL) {
		struct wlr_output_state st;
		wlr_output_state_init(&st);
		wlr_output_state_set_custom_mode(&st, 1280, 800, 30000);
		wlr_output_state_set_enabled(&st, true);
		if (wlr_output_commit_state(anchor, &st)) {
			wlr_output_create_global(anchor, server.display);
		} else {
			wlr_log(WLR_ERROR, "anchor 输出 commit 失败");
		}
		wlr_output_state_finish(&st);
	}

	// 虚拟输入管理器：注入客户端经协议创建设备。
	wlr_virtual_pointer_manager_v1_create(server.display);
	wlr_virtual_keyboard_manager_v1_create(server.display);

	// seat：指针 + 键盘能力。
	server.seat = wlr_seat_create(server.display, "seat0");
	wlr_seat_set_capabilities(server.seat,
			WL_SEAT_CAPABILITY_POINTER | WL_SEAT_CAPABILITY_KEYBOARD);

	// 数据设备（剪贴板转发的基础，v1 仅挂载）。
	wlr_data_device_manager_create(server.display);

	// 控制通道轮询。
	struct itimerspec its = {
		.it_interval = { .tv_sec = 0, .tv_nsec = 200000000 },
		.it_value = { .tv_sec = 0, .tv_nsec = 200000000 },
	};
	poll_timerfd = timerfd_create(CLOCK_MONOTONIC, 0);
	timerfd_settime(poll_timerfd, 0, &its, NULL);
	wl_event_loop_add_fd(loop, poll_timerfd, WL_EVENT_READABLE, poll_timer_cb, NULL);

	// wayland 套接字——注入客户端要用，必须先建。
	const char *wlsock = wl_display_add_socket_auto(server.display);
	if (wlsock == NULL) {
		wlr_log(WLR_ERROR, "wayland 套接字创建失败");
		return 1;
	}
	setenv("WAYLAND_DISPLAY", wlsock, 1);
	wlr_log(WLR_INFO, "wayland 套接字: %s", wlsock);

	if (!input_init(&server)) {
		wlr_log(WLR_ERROR, "注入客户端初始化失败");
		return 1;
	}
	if (!stream_connect(&server)) {
		wlr_log(WLR_ERROR, "mixumd 未就绪，将自动重连");
	}

	signal(SIGPIPE, SIG_IGN);
	wlr_log(WLR_INFO, "mixcomp 就绪：每窗口隔离合成器（wlroots 0.18）");
	wl_display_run(server.display);

	wl_display_destroy(server.display);
	return 0;
}
