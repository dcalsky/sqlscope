package sqlscope

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"runtime"
	"sync"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/api"
	"github.com/tetratelabs/wazero/imports/wasi_snapshot_preview1"

	"github.com/dcalsky/sqlscope/go/internal/wasm"
)

// abiVersion must match the module's sqlscope_abi_version.
const abiVersion = 1

// engine owns the compiled module and a pool of instances. A WebAssembly
// instance is single-threaded, so each concurrent call borrows one.
type engine struct {
	runtime  wazero.Runtime
	compiled wazero.CompiledModule
	idle     chan *instance
}

type instance struct {
	module api.Module
	alloc  api.Function
	free   api.Function
	call   api.Function
}

var (
	engineOnce   sync.Once
	sharedEngine *engine
	engineErr    error
)

// Init compiles the embedded SQL engine. It is optional: every function
// initializes the engine on first use. Calling it at startup moves the
// one-time compilation cost (around a second) out of the first request.
func Init() error {
	_, err := getEngine()
	return err
}

func getEngine() (*engine, error) {
	engineOnce.Do(func() {
		sharedEngine, engineErr = newEngine(context.Background())
	})
	return sharedEngine, engineErr
}

func newEngine(ctx context.Context) (*engine, error) {
	module, err := wasm.Module()
	if err != nil {
		return nil, fmt.Errorf("sqlscope: load engine: %w", err)
	}
	rt := wazero.NewRuntimeWithConfig(ctx, wazero.NewRuntimeConfig().WithCloseOnContextDone(false))
	if _, err := wasi_snapshot_preview1.Instantiate(ctx, rt); err != nil {
		rt.Close(ctx)
		return nil, fmt.Errorf("sqlscope: instantiate WASI: %w", err)
	}
	compiled, err := rt.CompileModule(ctx, module)
	if err != nil {
		rt.Close(ctx)
		return nil, fmt.Errorf("sqlscope: compile engine: %w", err)
	}
	e := &engine{runtime: rt, compiled: compiled, idle: make(chan *instance, runtime.GOMAXPROCS(0))}
	// Instantiate one eagerly to validate the module and its ABI.
	first, err := e.instantiate(ctx)
	if err != nil {
		rt.Close(ctx)
		return nil, err
	}
	e.release(first)
	return e, nil
}

func (e *engine) instantiate(ctx context.Context) (*instance, error) {
	module, err := e.runtime.InstantiateModule(ctx, e.compiled,
		wazero.NewModuleConfig().WithName("").WithStartFunctions("_initialize"))
	if err != nil {
		return nil, fmt.Errorf("sqlscope: instantiate engine: %w", err)
	}
	inst := &instance{
		module: module,
		alloc:  module.ExportedFunction("sqlscope_alloc"),
		free:   module.ExportedFunction("sqlscope_free"),
		call:   module.ExportedFunction("sqlscope_call"),
	}
	version := module.ExportedFunction("sqlscope_abi_version")
	if inst.alloc == nil || inst.free == nil || inst.call == nil || version == nil {
		module.Close(ctx)
		return nil, errors.New("sqlscope: engine is missing required exports")
	}
	results, err := version.Call(ctx)
	if err != nil || len(results) != 1 || results[0] != abiVersion {
		module.Close(ctx)
		return nil, fmt.Errorf("sqlscope: engine ABI mismatch (want %d)", abiVersion)
	}
	return inst, nil
}

func (e *engine) acquire(ctx context.Context) (*instance, error) {
	select {
	case inst := <-e.idle:
		return inst, nil
	default:
		return e.instantiate(ctx)
	}
}

func (e *engine) release(inst *instance) {
	select {
	case e.idle <- inst:
	default:
		inst.module.Close(context.Background())
	}
}

type response struct {
	Ok    json.RawMessage `json:"ok"`
	Error *Error          `json:"error"`
}

// UnmarshalJSON decodes the engine's error object.
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

// invoke runs one operation in the engine and decodes its result into out.
func invoke(operation string, request any, out any) error {
	e, err := getEngine()
	if err != nil {
		return &Error{Kind: KindInternal, Message: err.Error()}
	}
	payload, err := json.Marshal(request)
	if err != nil {
		return &Error{Kind: KindInvalidArgument, Message: "encode request: " + err.Error()}
	}
	ctx := context.Background()
	inst, err := e.acquire(ctx)
	if err != nil {
		return &Error{Kind: KindInternal, Message: err.Error()}
	}
	raw, err := inst.invoke(ctx, operation, payload)
	if err != nil {
		// A trap leaves the instance in an unknown state; discard it.
		inst.module.Close(ctx)
		return &Error{Kind: KindInternal, Message: "engine failure: " + err.Error()}
	}
	e.release(inst)

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

func (inst *instance) write(ctx context.Context, data []byte) (uint32, error) {
	results, err := inst.alloc.Call(ctx, uint64(len(data)))
	if err != nil {
		return 0, err
	}
	pointer := uint32(results[0])
	if !inst.module.Memory().Write(pointer, data) {
		return 0, errors.New("write out of bounds")
	}
	return pointer, nil
}

func (inst *instance) invoke(ctx context.Context, operation string, payload []byte) ([]byte, error) {
	op, err := inst.write(ctx, []byte(operation))
	if err != nil {
		return nil, err
	}
	defer inst.free.Call(ctx, uint64(op), uint64(len(operation)))
	req, err := inst.write(ctx, payload)
	if err != nil {
		return nil, err
	}
	defer inst.free.Call(ctx, uint64(req), uint64(len(payload)))

	results, err := inst.call.Call(ctx, uint64(op), uint64(len(operation)), uint64(req), uint64(len(payload)))
	if err != nil {
		return nil, err
	}
	pointer, length := uint32(results[0]>>32), uint32(results[0])
	view, ok := inst.module.Memory().Read(pointer, length)
	if !ok {
		return nil, errors.New("read out of bounds")
	}
	out := append([]byte(nil), view...)
	if _, err := inst.free.Call(ctx, uint64(pointer), uint64(length)); err != nil {
		return nil, err
	}
	return out, nil
}
