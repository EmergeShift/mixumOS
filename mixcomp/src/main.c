// main.c —— mixcomp 启动：headless 后端、xdg_shell、seat、
// 虚拟输入管理器、注入客户端、控制通道轮询。
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/timerfd.h>
#include <unistd.h>

#include <wlr/backend.h>
#include <wlr/render/wlr_renderer.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_data_device.h>
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

static void new_surface_cb(struct wl_listener *listener, void *data) {
	(void)listener;
	struct wlr_xdg_surface *xdg = data;
	if (xdg->role != WLR_XDG_SURFACE_ROLE_TOPLEVEL) {
		return;
	}
	// 常规合成器在此摆放窗口；mixcomp 只做隔离捕获。
	window_create(&server, xdg);
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
	wlr_log_init(WLR_INFO, NULL);

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
	setenv("WLR_BACKENDS", "headless", 0);
	setenv("WLR_RENDERER", "pixman", 0);
	setenv("WLR_LIBINPUT_NO_DEVICES", "1", 0);

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

	server.backend = wlr_backend_autocreate(loop, NULL);
	if (server.backend == NULL) {
		wlr_log(WLR_ERROR, "后端创建失败（需要 headless 支持）");
		return 1;
	}
	server.new_input.notify = new_input_cb;
	wl_signal_add(&server.backend->events.new_input, &server.new_input);
	server.renderer = wlr_renderer_autocreate(server.backend);
	if (server.renderer == NULL) {
		wlr_log(WLR_ERROR, "渲染器创建失败");
		return 1;
	}
	wl_display_init_shm(server.display);

	server.xdg_shell = wlr_xdg_shell_create(server.display, 3);
	server.new_surface.notify = new_surface_cb;
	wl_signal_add(&server.xdg_shell->events.new_surface, &server.new_surface);

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
