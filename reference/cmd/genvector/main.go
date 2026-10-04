// genvector —— 生成 Go 参考实现与 Rust 实现的交叉验证向量。
// 固定种子签发令牌,输出 JSON 供 rust/crates/mixum-protocol 的测试消费。
// 用法:go run ./cmd/genvector > ../../tests/cross-protocol/token-vector.json
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"

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
	out, _ := json.MarshalIndent(map[string]any{
		"seed_hex": fmt.Sprintf("%x", seed),
		"token":    tok,
		"expect": map[string]any{
			"sub":  "cross-vector",
			"caps": []string{"task.run", "snap.sync"},
		},
	}, "", "  ")
	fmt.Println(string(out))
}
