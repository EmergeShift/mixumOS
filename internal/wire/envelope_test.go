package wire

import (
	"encoding/json"
	"testing"
	"time"
)

func TestNewIDUniqueAndVersioned(t *testing.T) {
	seen := map[string]bool{}
	for i := 0; i < 1000; i++ {
		id := NewID()
		if seen[id] {
			t.Fatalf("id 重复: %s", id)
		}
		seen[id] = true
		// uuidv7: 第三组首字符为 7
		if id[14] != '7' {
			t.Fatalf("不是 uuidv7: %s", id)
		}
	}
}

func TestEnvelopeRoundTrip(t *testing.T) {
	e, err := NewEnvelope(OpTaskSubmit, TaskSubmitRequest{Instruction: "打开 b 站搜 mixumOS"})
	if err != nil {
		t.Fatal(err)
	}
	b, err := json.Marshal(e)
	if err != nil {
		t.Fatal(err)
	}
	got, err := Decode(b)
	if err != nil {
		t.Fatal(err)
	}
	if got.Op != OpTaskSubmit || got.ID != e.ID {
		t.Fatalf("往返不一致: %+v", got)
	}
	var req TaskSubmitRequest
	if err := got.ParsePayload(&req); err != nil {
		t.Fatal(err)
	}
	if req.Instruction != "打开 b 站搜 mixumOS" {
		t.Fatalf("负载不一致: %+v", req)
	}
}

func TestDecodeRejectsBadVersion(t *testing.T) {
	e, _ := NewEnvelope(OpSysPing, nil)
	e.V = 99
	b, _ := json.Marshal(e)
	if _, err := Decode(b); err == nil {
		t.Fatal("应拒绝未知协议版本")
	}
}

func TestVideoFrameRoundTrip(t *testing.T) {
	payload := []byte("jpegdata测试")
	buf := MarshalVideo(42, 1920, 1080, true, -3, 700, 24, 24, true, payload)
	f, err := UnmarshalVideo(buf)
	if err != nil {
		t.Fatal(err)
	}
	if f.WindowID != 42 || f.Width != 1920 || f.Height != 1080 || !f.Keyframe || !f.HasCursor {
		t.Fatalf("头部不一致: %+v", f)
	}
	if f.CursorX != -3 || f.CursorY != 700 || f.CursorW != 24 || f.CursorH != 24 {
		t.Fatalf("光标字段不一致: %+v", f)
	}
	if string(f.Data) != string(payload) {
		t.Fatalf("帧数据不一致")
	}
}

func TestVideoFrameRejectsTruncated(t *testing.T) {
	if _, err := UnmarshalVideo([]byte{ChanVideo, 0, 0}); err == nil {
		t.Fatal("应拒绝截断帧")
	}
}

func TestReplyCarriesReplyTo(t *testing.T) {
	e, _ := NewEnvelope(OpSnapTree, nil)
	r, err := e.Reply(OpSnapTree+".ok", SnapTreeResponse{Head: "n1"})
	if err != nil {
		t.Fatal(err)
	}
	if r.ReplyTo != e.ID {
		t.Fatalf("replyTo 丢失")
	}
	_ = time.Now()
}
