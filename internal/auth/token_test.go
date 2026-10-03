package auth

import (
	"testing"
	"time"

	"github.com/mixumos/mixumos/internal/wire"
)

func newSignerVerifier(t *testing.T, jtiTTL time.Duration) (*Signer, *Verifier) {
	t.Helper()
	seed, err := GenerateSeed()
	if err != nil {
		t.Fatal(err)
	}
	s := NewSigner(seed)
	return s, NewVerifier(s.PublicKey(), jtiTTL)
}

func TestTokenMintVerify(t *testing.T) {
	s, v := newSignerVerifier(t, 0)
	tok, err := s.Mint(Claims{Sub: "iphone-15", Caps: []wire.Cap{wire.CapTaskRun, wire.CapDisplayView}})
	if err != nil {
		t.Fatal(err)
	}
	c, err := v.Verify(tok)
	if err != nil {
		t.Fatal(err)
	}
	if c.Sub != "iphone-15" || !HasCap(c, wire.CapTaskRun) || HasCap(c, wire.CapAdmin) {
		t.Fatalf("claims 不一致: %+v", c)
	}
}

func TestTokenRejectsTampered(t *testing.T) {
	s, v := newSignerVerifier(t, 0)
	tok, _ := s.Mint(Claims{Sub: "x", Caps: []wire.Cap{wire.CapAdmin}})
	// 篡改载荷中的 sub（翻转末字符）
	bad := tok[:len(tok)-4] + "AAAA"
	if _, err := v.Verify(bad); err == nil {
		t.Fatal("被篡改的令牌应被拒绝")
	}
	// 用另一把钥匙签发
	seed2, _ := GenerateSeed()
	other := NewSigner(seed2)
	tok2, _ := other.Mint(Claims{Sub: "y"})
	if _, err := v.Verify(tok2); err == nil {
		t.Fatal("非本服务签发的令牌应被拒绝")
	}
}

func TestTokenExpiry(t *testing.T) {
	s, v := newSignerVerifier(t, 0)
	tok, _ := s.Mint(Claims{Sub: "x", Exp: time.Now().Add(-time.Minute).Unix()})
	if _, err := v.Verify(tok); err == nil {
		t.Fatal("过期令牌应被拒绝")
	}
}

func TestTokenReuseAcrossConnections(t *testing.T) {
	s, v := newSignerVerifier(t, time.Minute)
	tok, _ := s.Mint(Claims{Sub: "x"})
	// 长期令牌允许跨连接复用（TLS 防传输重放，jti 留给吊销列表）。
	for i := 0; i < 3; i++ {
		if _, err := v.Verify(tok); err != nil {
			t.Fatalf("第 %d 次复用被拒: %v", i+1, err)
		}
	}
}

func TestWindowAllowed(t *testing.T) {
	c := &Claims{Windows: []uint32{1, 3}}
	if !WindowAllowed(c, 3) || WindowAllowed(c, 2) {
		t.Fatal("窗口限定判断错误")
	}
	all := &Claims{}
	if !WindowAllowed(all, 9) {
		t.Fatal("未限定窗口应放行全部")
	}
}
