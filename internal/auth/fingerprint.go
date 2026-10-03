package auth

import (
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"fmt"
	"strings"
)

// FormatFingerprint 把 DER 证书编码为冒号分隔的 SHA-256 指纹。
func FormatFingerprint(der []byte) string {
	sum := sha256.Sum256(der)
	parts := make([]string, len(sum))
	for i, b := range sum {
		parts[i] = hex.EncodeToString([]byte{b})
	}
	return strings.Join(parts, ":")
}

// PinVerifier 返回一个固定指纹的 tls.Verifier 风格函数：证书指纹不匹配即拒绝。
// 用于主机端首次连接后固定（TOFU）与后续强校验。
func PinVerifier(fingerprint string) func(rawCerts [][]byte) error {
	return func(rawCerts [][]byte) error {
		if len(rawCerts) == 0 {
			return fmt.Errorf("auth: 服务器未提供证书")
		}
		got := FormatFingerprint(rawCerts[0])
		if got != fingerprint {
			return fmt.Errorf("auth: 证书指纹不匹配（可能被中间人替换）：got %s want %s", got, fingerprint)
		}
		return nil
	}
}

// ClientTLSConfig 构造客户端 TLS 配置：不使用系统 CA，而是固定指纹校验。
func ClientTLSConfig(fingerprint string) *tls.Config {
	return &tls.Config{
		MinVersion:         tls.VersionTLS13,
		InsecureSkipVerify: true, // 由 VerifyPeerCertificate 里的指纹固定替代系统 CA 校验
		VerifyPeerCertificate: func(rawCerts [][]byte, _ [][]*x509.Certificate) error {
			return PinVerifier(fingerprint)(rawCerts)
		},
	}
}

// ServerTLSConfig 构造服务端 TLS 配置：仅 TLS 1.3，强密码套件。
func ServerTLSConfig(cert *tls.Certificate) *tls.Config {
	return &tls.Config{
		MinVersion:   tls.VersionTLS13,
		Certificates: []tls.Certificate{*cert},
	}
}
