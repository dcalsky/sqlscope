package sqlscope

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"sync/atomic"
	"unsafe"

	"github.com/ebitengine/purego"
)

// LibraryPathEnv names the environment variable that locates the sqlscope
// FFI library when Load is not given a path.
const LibraryPathEnv = "SQLSCOPE_LIBRARY_PATH"

// abiVersion must match the library's sqlscope_abi_version.
const abiVersion = 1

// library is a loaded sqlscope FFI library. It is never unloaded.
type library struct {
	path    string
	version string
	call    func(operation, request string) *byte
	free    func(*byte)
}

var (
	loaded atomic.Pointer[library]
	loadMu sync.Mutex
)

// LibraryFileName is the platform's file name of the sqlscope shared
// library: libsqlscope_ffi.so, libsqlscope_ffi.dylib or sqlscope_ffi.dll.
func LibraryFileName() string {
	switch runtime.GOOS {
	case "darwin", "ios":
		return "libsqlscope_ffi.dylib"
	case "windows":
		return "sqlscope_ffi.dll"
	default:
		return "libsqlscope_ffi.so"
	}
}

// Load loads the sqlscope FFI library, the shared library published with
// each sqlscope release. With an empty path it tries, in order, the path in
// $SQLSCOPE_LIBRARY_PATH, LibraryFileName next to the executable, and
// LibraryFileName on the system library search path.
//
// Load is optional: the first operation loads the library the same way.
// Calling it at startup selects the library explicitly and reports a
// missing library early. Once a library is loaded it stays loaded; loading
// a different path afterwards is an error.
func Load(path string) (err error) {
	loadMu.Lock()
	defer loadMu.Unlock()
	if lib := loaded.Load(); lib != nil {
		if path == "" || path == lib.path {
			return nil
		}
		return fmt.Errorf("sqlscope: library already loaded from %s", lib.path)
	}
	var lib *library
	if path != "" {
		lib, err = openLibrary(path)
	} else {
		lib, err = openDefault()
	}
	if err != nil {
		return err
	}
	loaded.Store(lib)
	return nil
}

// LibraryVersion returns the version of the loaded sqlscope FFI library,
// loading it if needed.
func LibraryVersion() (string, error) {
	lib, err := getLibrary()
	if err != nil {
		return "", err
	}
	return lib.version, nil
}

func getLibrary() (*library, error) {
	if lib := loaded.Load(); lib != nil {
		return lib, nil
	}
	if err := Load(""); err != nil {
		return nil, err
	}
	return loaded.Load(), nil
}

func openDefault() (*library, error) {
	name := LibraryFileName()
	var candidates []string
	if path := os.Getenv(LibraryPathEnv); path != "" {
		candidates = append(candidates, path)
	} else {
		if exe, err := os.Executable(); err == nil {
			candidates = append(candidates, filepath.Join(filepath.Dir(exe), name))
		}
		candidates = append(candidates, name)
	}
	var errs []error
	for _, candidate := range candidates {
		lib, err := openLibrary(candidate)
		if err == nil {
			return lib, nil
		}
		errs = append(errs, err)
	}
	return nil, fmt.Errorf("sqlscope: cannot load %s; download it from a sqlscope release and pass its path to Load or set %s: %w",
		name, LibraryPathEnv, errors.Join(errs...))
}

func openLibrary(path string) (*library, error) {
	handle, err := openHandle(path)
	if err != nil {
		return nil, fmt.Errorf("sqlscope: open %s: %w", path, err)
	}
	var (
		abi     func() uint32
		version func() string
		lib     = &library{path: path}
	)
	for _, symbol := range []struct {
		name string
		fn   any
	}{
		{"sqlscope_abi_version", &abi},
		{"sqlscope_version", &version},
		{"sqlscope_call", &lib.call},
		{"sqlscope_free", &lib.free},
	} {
		address, err := lookupSymbol(handle, symbol.name)
		if err != nil {
			return nil, fmt.Errorf("sqlscope: %s: missing symbol %s: %w", path, symbol.name, err)
		}
		purego.RegisterFunc(symbol.fn, address)
	}
	if got := abi(); got != abiVersion {
		return nil, fmt.Errorf("sqlscope: %s implements ABI %d, want %d", path, got, abiVersion)
	}
	lib.version = version()
	return lib, nil
}

type response struct {
	Ok    json.RawMessage `json:"ok"`
	Error *Error          `json:"error"`
}

// UnmarshalJSON decodes the library's error object.
func (e *Error) UnmarshalJSON(data []byte) error {
	var raw struct {
		Kind    Kind   `json:"kind"`
		Message string `json:"message"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	e.Kind, e.Message = raw.Kind, raw.Message
	return nil
}

// invoke runs one operation in the library and decodes its result into out.
func invoke(operation string, request any, out any) error {
	lib, err := getLibrary()
	if err != nil {
		return &Error{Kind: KindInternal, Message: err.Error()}
	}
	payload, err := json.Marshal(request)
	if err != nil {
		return &Error{Kind: KindInvalidArgument, Message: "encode request: " + err.Error()}
	}
	raw := lib.invoke(operation, string(payload))

	var resp response
	if err := json.Unmarshal(raw, &resp); err != nil {
		return &Error{Kind: KindInternal, Message: "decode response: " + err.Error()}
	}
	if resp.Error != nil {
		return resp.Error
	}
	if err := json.Unmarshal(resp.Ok, out); err != nil {
		return &Error{Kind: KindInternal, Message: "decode result: " + err.Error()}
	}
	return nil
}

// invoke calls sqlscope_call and copies the NUL-terminated response.
func (lib *library) invoke(operation, request string) []byte {
	response := lib.call(operation, request)
	defer lib.free(response)
	n := 0
	for *(*byte)(unsafe.Add(unsafe.Pointer(response), n)) != 0 {
		n++
	}
	return append([]byte(nil), unsafe.Slice(response, n)...)
}
