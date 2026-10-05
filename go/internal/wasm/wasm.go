// Package wasm embeds the sqlscope WebAssembly module.
//
// The module is built from the Rust sources by scripts/build-wasm.sh.
package wasm

import (
	"bytes"
	"compress/gzip"
	_ "embed"
	"io"
)

//go:embed sqlscope.wasm.gz
var compressed []byte

// Module returns the uncompressed WebAssembly module.
func Module() ([]byte, error) {
	reader, err := gzip.NewReader(bytes.NewReader(compressed))
	if err != nil {
		return nil, err
	}
	defer reader.Close()
	return io.ReadAll(reader)
}
