// clipboard.c —— 剪贴板同步（console ↔ 容器 Wayland 选区）。
// 下行：console 推来的文本包成内建 wlr_data_source 设为 seat selection，
//   供容器应用粘贴（wlroots data-control 内部 source 同款模式）；
// 上行：容器应用复制触发 request_set_selection，经 wlr_data_source_send
//   把文本写入管道，event loop 读回 EOF 后发 JSON 控制消息上行。
// 生命周期：source 一律由 wlroots 拥有——被替换时 wlr_seat_set_selection
//   内部调 wlr_data_source_destroy（发 destroy 信号、释放 mime、调
//   impl->destroy）；本文件只在 seat set_selection 信号里同步指针。
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <wlr/types/wlr_data_device.h>

#include "mixcomp.h"

// 单次读回上限 4MiB：JSON 转义最坏 6 倍膨胀仍低于 16MiB 控制帧。
#define CLIP_MAX (4u << 20)
#define CLIP_CONTROL_MAX ((16u << 20) - 1024)

struct clip_source {
	struct wlr_data_source base;
	char *text;
	size_t len;
};

struct clip_read {
	struct mixcomp_server *server;
	struct wl_event_source *src;
	int fd;
	uint32_t window_id;
	uint8_t *buf;
	size_t len, cap;
};

static void clip_source_send(struct wlr_data_source *base, const char *mime_type,
		int32_t fd) {
	(void)mime_type; // 只声明 text/*，请求哪个 mime 都给同一份文本
	struct clip_source *src = wl_container_of(base, src, base);
	size_t off = 0;
	while (off < src->len) {
		ssize_t n = write(fd, src->text + off, src->len - off);
		if (n > 0) {
			off += (size_t)n;
			continue;
		}
		if (n < 0 && (errno == EINTR || errno == EAGAIN)) {
			continue;
		}
		break;
	}
	close(fd);
}

static void clip_source_accept(struct wlr_data_source *base, uint32_t serial,
		const char *mime_type) {
	(void)base; (void)serial; (void)mime_type;
}

// wlroots 释放 source 时调用：base 结构体及其上的扩展字段一起释放。
static void clip_source_destroy(struct wlr_data_source *base) {
	struct clip_source *src = wl_container_of(base, src, base);
	free(src->text);
	free(src);
}

static const struct wlr_data_source_impl clip_source_impl = {
	.send = clip_source_send,
	.accept = clip_source_accept,
	.destroy = clip_source_destroy,
};

// clipboard_set —— console→容器：把文本设为 seat selection。
void clipboard_set(struct mixcomp_server *s, const char *text) {
	size_t len = strlen(text);
	if (len == 0 || len > CLIP_MAX) {
		return;
	}
	struct clip_source *src = calloc(1, sizeof(*src));
	if (src == NULL) {
		return;
	}
	src->text = strdup(text);
	src->len = len;
	if (src->text == NULL) {
		free(src);
		return;
	}
	// init 会整体重置 base（含 mime_types），mime 必须在其后添加。
	wlr_data_source_init(&src->base, &clip_source_impl);
	bool mimes_ok = true;
	const char *mimes[] = {"text/plain;charset=utf-8", "text/plain"};
	for (size_t i = 0; i < sizeof(mimes) / sizeof(mimes[0]); i++) {
		char **slot = wl_array_add(&src->base.mime_types, sizeof(char *));
		char *dup = slot != NULL ? strdup(mimes[i]) : NULL;
		if (slot == NULL || dup == NULL) {
			free(dup);
			mimes_ok = false;
			break;
		}
		*slot = dup;
	}
	if (!mimes_ok) {
		wlr_data_source_destroy(&src->base);
		return;
	}

	// 旧 source（我们的或客户端的）由 wlroots 在 set 内部销毁，
	// seat set_selection 信号回调里同步本文件指针。
	// serial 必须取自 display 权威计数器：自增小计数会超过 display
	// serial,客户端随后的 set_selection(serial=next_serial) 因无符号
	// 下溢被 wlroots 当作过期请求静默丢弃。
	wlr_seat_set_selection(s->seat, &src->base,
			wl_display_next_serial(s->display));
	s->clip_source = src;
	wlr_log(WLR_INFO, "剪贴板下行 %zu 字节已设为选区", len);
}

// clip_json_escaped_len —— 估算 json-c 字符串转义后长度；含 NUL 时返回
// (size_t)-1（二进制内容不能作为 JSON 文本上行）。
static size_t clip_json_escaped_len(const uint8_t *buf, size_t len) {
	size_t total = 0;
	for (size_t i = 0; i < len; i++) {
		uint8_t b = buf[i];
		if (b == '\0') {
			return (size_t)-1;
		}
		total += (b == '"' || b == '\\') ? 2 : (b < 0x20 ? 6 : 1);
	}
	return total;
}

// clip_read_finish —— 读回收尾：send=true 时把累积文本上行。
static void clip_read_finish(struct clip_read *cr, bool send) {
	wlr_log(WLR_INFO, "选区读回收尾:%zu 字节,上行=%d", cr->len, send);
	if (cr->server->clip_read == cr) {
		cr->server->clip_read = NULL;
	}
	if (cr->src != NULL) {
		wl_event_source_remove(cr->src);
	}
	close(cr->fd);
	if (send && cr->len > 0 && cr->len <= CLIP_MAX) {
		size_t escaped = clip_json_escaped_len(cr->buf, cr->len);
		if (escaped == (size_t)-1 || escaped > CLIP_CONTROL_MAX) {
			wlr_log(WLR_INFO, "选区上行丢弃:%zu 字节转义后超过控制帧", cr->len);
			send = false;
		}
	}
	if (send && cr->len > 0 && cr->len <= CLIP_MAX) {
		cr->buf[cr->len] = '\0';
		stream_send_clipboard(cr->server, cr->window_id,
				(const char *)cr->buf);
	}
	free(cr->buf);
	free(cr);
}

static int clip_read_cb(int fd, uint32_t mask, void *data) {
	(void)fd;
	struct clip_read *cr = data;
	bool done = (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) != 0;
	for (;;) {
		if (cr->len + 1 >= cr->cap) {
			if (cr->cap >= CLIP_MAX + 2) {
				clip_read_finish(cr, false); // 超限：丢弃，fd 不再读免忙轮询
				return 0;
			}
			size_t ncap = cr->cap ? cr->cap * 2 : 4096;
			if (ncap > CLIP_MAX + 2) {
				ncap = CLIP_MAX + 2;
			}
			uint8_t *nb = realloc(cr->buf, ncap);
			if (nb == NULL) {
				clip_read_finish(cr, false);
				return 0;
			}
			cr->buf = nb;
			cr->cap = ncap;
		}
		ssize_t n = read(cr->fd, cr->buf + cr->len, cr->cap - 1 - cr->len);
		if (n > 0) {
			cr->len += (size_t)n;
			continue;
		}
		if (n < 0 && errno == EINTR) {
			continue;
		}
		if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
			break;
		}
		done = true; // EOF 或读错误
		break;
	}
	if (done) {
		clip_read_finish(cr, true);
	}
	return 0;
}

// request_set_selection_cb —— 容器应用复制：接受选区并异步读回文本。
static void request_set_selection_cb(struct wl_listener *l, void *data) {
	struct mixcomp_server *s = wl_container_of(l, s, clip_sel_req);
	struct wlr_seat_request_set_selection_event *ev = data;
	wlr_seat_set_selection(s->seat, ev->source, ev->serial);
	if (ev->source == NULL) {
		return;
	}

	// mime 协商：优先带 charset 的 UTF-8 明文，其次任意 text/plain 前缀。
	const char *mime = NULL;
	char **types;
	wl_array_for_each(types, &ev->source->mime_types) {
		if (*types != NULL && strcmp(*types, "text/plain;charset=utf-8") == 0) {
			mime = *types;
			break;
		}
	}
	if (mime == NULL) {
		wl_array_for_each(types, &ev->source->mime_types) {
			if (*types != NULL && strncmp(*types, "text/plain", 10) == 0) {
				mime = *types;
				break;
			}
		}
	}
	if (mime == NULL) {
		wlr_log(WLR_INFO, "选区上行忽略:非文本 mime(共 %zu 个)",
				ev->source->mime_types.size / sizeof(char *));
		return; // 非文本选区不同步
	}
	// v1 单选区:用事件发生时的焦点窗口归属上行文本。无焦点时若只有一个
	// 窗口,归属该窗口;多窗口且无焦点则无法套用窗口 ACL,宁可不上报。
	struct mixcomp_window *focus = s->focus_window;
	if (focus == NULL) {
		struct mixcomp_window *only = NULL;
		size_t count = 0;
		struct mixcomp_window *w;
		wl_list_for_each(w, &s->windows, link) {
			only = w;
			count++;
		}
		if (count == 1) {
			focus = only;
		}
	}
	if (focus == NULL) {
		wlr_log(WLR_INFO, "选区上行忽略:无焦点窗口可归属");
		return;
	}
	wlr_log(WLR_INFO, "选区上行开始:mime=%s", mime);

	int fds[2];
	if (pipe(fds) != 0) {
		return;
	}
	int flags = fcntl(fds[0], F_GETFL, 0);
	fcntl(fds[0], F_SETFL, flags | O_NONBLOCK);
	fcntl(fds[1], F_SETFD, FD_CLOEXEC);

	// 上一轮读回未完（选区连续变化）：丢弃旧数据，保证上行最新。
	if (s->clip_read != NULL) {
		clip_read_finish(s->clip_read, false);
	}
	struct clip_read *cr = calloc(1, sizeof(*cr));
	if (cr == NULL) {
		close(fds[0]);
		close(fds[1]);
		return;
	}
	cr->server = s;
	cr->fd = fds[0];
	cr->window_id = focus->id;
	s->clip_read = cr;

	wlr_data_source_send(ev->source, mime, fds[1]);
	close(fds[1]); // 客户端持有经 wayland 传递的副本，本端立即关闭

	struct wl_event_loop *loop = wl_display_get_event_loop(s->display);
	cr->src = wl_event_loop_add_fd(loop, fds[0], WL_EVENT_READABLE,
			clip_read_cb, cr);
	if (cr->src == NULL) {
		clip_read_finish(cr, false);
	}
}

// set_selection_cb —— 选区变化后同步内建 source 指针：被顶掉时 source
// 已由 wlroots 销毁（wlr_seat_set_selection / 客户端资源释放路径），只清指针。
static void set_selection_cb(struct wl_listener *l, void *data) {
	(void)data;
	struct mixcomp_server *s = wl_container_of(l, s, clip_sel_set);
	if (s->clip_source != NULL &&
			s->seat->selection_source != &s->clip_source->base) {
		s->clip_source = NULL;
	}
}

void clipboard_init(struct mixcomp_server *s) {
	s->clip_sel_req.notify = request_set_selection_cb;
	wl_signal_add(&s->seat->events.request_set_selection, &s->clip_sel_req);
	s->clip_sel_set.notify = set_selection_cb;
	wl_signal_add(&s->seat->events.set_selection, &s->clip_sel_set);
}
