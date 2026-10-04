// Package auth 实现 mixumOS 的通信安全原语：
// ed25519 签名的能力令牌、服务端身份密钥、自签 CA 与证书指纹固定。
// 设计见 docs/security.md。
package auth

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/mixumos/mixumos/internal/wire"
)

// Claims 是能力令牌的载荷。令牌是 `mixum1.<b64(payload)>.<b64(sig)>` 格式，
// 由服务端身份密钥签名，无状态可验证。
type Claims struct {
	Sub     string     `json:"sub"`               // 主体：设备或用户标识
	Caps    []wire.Cap `json:"caps"`              // 授予的能力集合
	Exp     int64      `json:"exp"`               // 过期时间 unix 秒
	Iat     int64      `json:"iat"`
	Jti     string     `json:"jti"`               // 唯一 id，用于去重
	TaskID  string     `json:"taskId,omitempty"`  // 限定可操作的任务
	Windows []uint32   `json:"windows,omitempty"` // 限定可观看/接管的窗口
}

const tokenPrefix = "mixum1."

// Signer 用 ed25519 私钥签发令牌。
type Signer struct {
	key ed25519.PrivateKey
}

// NewSigner 从种子构造。
func NewSigner(seed []byte) *Signer {
	return &Signer{key: ed25519.NewKeyFromSeed(seed)}
}

// GenerateSeed 生成新种子。
func GenerateSeed() ([]byte, error) {
	seed := make([]byte, ed25519.SeedSize)
	if _, err := rand.Read(seed); err != nil {
		return nil, err
	}
	return seed, nil
}

// PublicKey 返回公钥。
func (s *Signer) PublicKey() ed25519.PublicKey {
	return s.key.Public().(ed25519.PublicKey)
}

// Mint 签发一个令牌。
func (s *Signer) Mint(c Claims) (string, error) {
	if c.Jti == "" {
		c.Jti = wire.NewID()
	}
	if c.Iat == 0 {
		c.Iat = time.Now().Unix()
	}
	if c.Exp == 0 {
		c.Exp = c.Iat + 3600
	}
	payload, err := json.Marshal(c)
	if err != nil {
		return "", err
	}
	sig := ed25519.Sign(s.key, payload)
	return tokenPrefix +
		base64.RawURLEncoding.EncodeToString(payload) + "." +
		base64.RawURLEncoding.EncodeToString(sig), nil
}

// Verify 验证令牌签名与有效期。
// jti 仅记录在 claims 中供将来吊销列表使用；长期令牌允许跨连接复用，
// 传输层重放由 TLS 1.3 与会话机制防护（docs/security.md）。
type Verifier struct {
	pub ed25519.PublicKey
}

// NewVerifier 构造验证器。
func NewVerifier(pub ed25519.PublicKey, _ time.Duration) *Verifier {
	return &Verifier{pub: pub}
}

// Verify 校验一个令牌。
func (v *Verifier) Verify(raw string) (*Claims, error) {
	raw = strings.TrimSpace(raw)
	if !strings.HasPrefix(raw, tokenPrefix) {
		return nil, fmt.Errorf("auth: 令牌格式不正确")
	}
	rest := raw[len(tokenPrefix):]
	dot := strings.LastIndexByte(rest, '.')
	if dot < 0 {
		return nil, fmt.Errorf("auth: 令牌缺少签名段")
	}
	payload, err := base64.RawURLEncoding.DecodeString(rest[:dot])
	if err != nil {
		return nil, fmt.Errorf("auth: 载荷 base64 解码失败: %w", err)
	}
	sig, err := base64.RawURLEncoding.DecodeString(rest[dot+1:])
	if err != nil {
		return nil, fmt.Errorf("auth: 签名 base64 解码失败: %w", err)
	}
	if !ed25519.Verify(v.pub, payload, sig) {
		return nil, fmt.Errorf("auth: 令牌签名无效")
	}
	var c Claims
	if err := json.Unmarshal(payload, &c); err != nil {
		return nil, fmt.Errorf("auth: 载荷解析失败: %w", err)
	}
	now := time.Now().Unix()
	if now > c.Exp {
		return nil, fmt.Errorf("auth: 令牌已过期")
	}
	return &c, nil
}

// HasCap 判断 claims 是否包含某能力。admin 能力覆盖全部。
func HasCap(c *Claims, need wire.Cap) bool {
	for _, cp := range c.Caps {
		if cp == need || cp == wire.CapAdmin {
			return true
		}
	}
	return false
}

// WindowAllowed 判断 claims 是否覆盖某窗口（未限定窗口则全覆盖）。
func WindowAllowed(c *Claims, id uint32) bool {
	if len(c.Windows) == 0 {
		return true
	}
	for _, w := range c.Windows {
		if w == id {
			return true
		}
	}
	return false
}
