// genvector —— 生成 Go 参考实现与 Rust 实现的交叉验证向量。
// 固定种子签发令牌,输出 JSON 供 rust/crates/mixum-protocol 的测试消费。
// 用法:go run ./cmd/genvector > ../../tests/cross-protocol/snapshot-vector.json
package main

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"crypto/hkdf"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"

	"github.com/klauspost/compress/zstd"
	"github.com/mixumos/mixumos/internal/auth"
	"github.com/mixumos/mixumos/internal/wire"
)

func main() {
	seed := bytes.Repeat([]byte{0xAB}, 32)
	signer := auth.NewSigner(seed)
	tok, err := signer.Mint(auth.Claims{
		Sub:  "cross-vector",
		Caps: []wire.Cap{wire.CapTaskRun, wire.CapSnapSync},
		Iat:  1700000000,
		Exp:  4102444800, // 2100-01-01,向量长期有效
	})
	if err != nil {
		fmt.Fprintln(os.Stderr, "mint:", err)
		os.Exit(1)
	}
	plain := map[string][]byte{
		"hello.txt": []byte("mixum snapshot cross vector\n"),
		"data.bin":  []byte("固定块内容：v1"),
	}
	chunks := map[string]string{}
	for name, data := range plain {
		h := sha256.Sum256(data)
		chunks[name] = hex.EncodeToString(h[:])
	}
	// 固定 nonce 仅用于交叉测试；生产 Store 仍使用随机 nonce。
	master := bytes.Repeat([]byte{0x11}, 32)
	storeID := "vector-store"
	hash := chunks["hello.txt"]
	key, err := hkdf.Key(sha256.New, master, []byte("mixum/snap/v1:"+storeID), "chunk-aead", 32)
	if err != nil {
		panic(err)
	}
	bl, err := aes.NewCipher(key)
	if err != nil {
		panic(err)
	}
	aead, err := cipher.NewGCM(bl)
	if err != nil {
		panic(err)
	}
	nonce := []byte{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11}
	enc, err := zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.SpeedFastest))
	if err != nil {
		panic(err)
	}
	compressed := enc.EncodeAll(plain["hello.txt"], nil)
	aad := []byte("mixum-chunk:" + storeID + ":" + hash)
	ciphertext := aead.Seal(nil, nonce, compressed, aad)
	out, _ := json.MarshalIndent(map[string]any{
		"seed_hex": fmt.Sprintf("%x", seed),
		"token":    tok,
		"manifest": map[string]any{
			"version": 1,
			"files": []map[string]any{
				{"path": "data.bin", "mode": uint32(0o644), "size": len(plain["data.bin"]), "chunks": []string{chunks["data.bin"]}},
				{"path": "hello.txt", "mode": uint32(0o644), "size": len(plain["hello.txt"]), "chunks": []string{chunks["hello.txt"]}},
			},
			"dirs": []map[string]any{{"path": "docs", "mode": uint32(0o755)}},
		},
		"chunks": map[string]any{
			chunks["hello.txt"]: map[string]string{"name": "hello.txt", "plain_hex": hex.EncodeToString(plain["hello.txt"])},
			chunks["data.bin"]:  map[string]string{"name": "data.bin", "plain_hex": hex.EncodeToString(plain["data.bin"])},
		},
		"gcm": map[string]string{
			"store_id": storeID, "master_key_hex": hex.EncodeToString(master), "nonce_hex": hex.EncodeToString(nonce),
			"hash": hash, "ciphertext_hex": hex.EncodeToString(append(nonce, ciphertext...)),
		},
		"expect": map[string]any{
			"sub":  "cross-vector",
			"caps": []string{"task.run", "snap.sync"},
		},
	}, "", "  ")
	fmt.Println(string(out))
}
