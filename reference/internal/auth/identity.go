package auth

import (
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"fmt"
	"math/big"
	"net"
	"os"
	"path/filepath"
	"time"
)

// Identity 是 mixumd 的长期身份：令牌签名密钥 + TLS 证书。
// 身份文件保存在数据目录，首次启动自动生成。
type Identity struct {
	Seed    []byte
	TLS     *tls.Certificate
	Fingerprint string // SHA-256 指纹的 hex 冒号分隔形式，供主机端固定（TOFU）
}

const (
	seedFile  = "identity.key"
	certFile  = "tls-cert.pem"
	keyFile   = "tls-key.pem"
)

// LoadOrCreate 加载或生成身份。hosts 是证书 SAN 列表。
func LoadOrCreate(dataDir string, hosts []string) (*Identity, error) {
	if err := os.MkdirAll(dataDir, 0o700); err != nil {
		return nil, err
	}
	seed, err := readOrCreateSeed(filepath.Join(dataDir, seedFile))
	if err != nil {
		return nil, err
	}
	cert, fp, err := readOrCreateCert(dataDir, hosts)
	if err != nil {
		return nil, err
	}
	return &Identity{Seed: seed, TLS: cert, Fingerprint: fp}, nil
}

func readOrCreateSeed(path string) ([]byte, error) {
	if b, err := os.ReadFile(path); err == nil && len(b) == ed25519.SeedSize {
		return b, nil
	}
	seed, err := GenerateSeed()
	if err != nil {
		return nil, err
	}
	if err := os.WriteFile(path, seed, 0o600); err != nil {
		return nil, err
	}
	return seed, nil
}

// readOrCreateCert 生成自签证书（TLS 1.3 需要）。返回指纹。
func readOrCreateCert(dir string, hosts []string) (*tls.Certificate, string, error) {
	certPath := filepath.Join(dir, certFile)
	keyPath := filepath.Join(dir, keyFile)
	if _, err := os.Stat(certPath); err == nil {
		cert, err := tls.LoadX509KeyPair(certPath, keyPath)
		if err == nil {
			fp, err := fingerprintFile(certPath)
			return &cert, fp, err
		}
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, "", err
	}
	serial, _ := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 120))
	tmpl := x509.Certificate{
		SerialNumber: serial,
		Subject:      pkix.Name{CommonName: "mixumd"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().AddDate(10, 0, 0),
		KeyUsage:     x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	for _, h := range hosts {
		if ip := net.ParseIP(h); ip != nil {
			tmpl.IPAddresses = append(tmpl.IPAddresses, ip)
		} else {
			tmpl.DNSNames = append(tmpl.DNSNames, h)
		}
	}
	der, err := x509.CreateCertificate(rand.Reader, &tmpl, &tmpl, &key.PublicKey, key)
	if err != nil {
		return nil, "", err
	}
	keyDER, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		return nil, "", err
	}
	if err := os.WriteFile(certPath, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), 0o644); err != nil {
		return nil, "", err
	}
	if err := os.WriteFile(keyPath, pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER}), 0o600); err != nil {
		return nil, "", err
	}
	cert, err := tls.LoadX509KeyPair(certPath, keyPath)
	if err != nil {
		return nil, "", err
	}
	fp, err := fingerprintFile(certPath)
	return &cert, fp, err
}

func fingerprintFile(path string) (string, error) {
	pemBytes, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	block, _ := pem.Decode(pemBytes)
	if block == nil {
		return "", fmt.Errorf("auth: 证书 PEM 解析失败")
	}
	crt, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return "", err
	}
	return FormatFingerprint(crt.Raw), nil
}
