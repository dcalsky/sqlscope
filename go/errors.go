package sqlscope

import "errors"

// Kind classifies an Error.
type Kind string

const (
	// KindInvalidArgument: an option or argument is invalid.
	KindInvalidArgument Kind = "invalid_argument"
	// KindParse: the SQL text could not be parsed.
	KindParse Kind = "parse"
	// KindUnsupported: the statement shape is not supported, or the input
	// exceeded a safety limit.
	KindUnsupported Kind = "unsupported"
	// KindInternal: sqlscope produced an invalid result, or its library
	// could not be loaded.
	KindInternal Kind = "internal"
)

// Sentinel errors for errors.Is.
var (
	ErrInvalidArgument = errors.New("sqlscope: invalid argument")
	ErrParse           = errors.New("sqlscope: parse error")
	ErrUnsupported     = errors.New("sqlscope: unsupported")
	ErrInternal        = errors.New("sqlscope: internal error")
)

// Error is returned by every sqlscope function. Use errors.Is with the
// sentinel errors, or inspect Kind.
type Error struct {
	Kind    Kind
	Message string
}

func (e *Error) Error() string {
	return e.sentinel().Error() + ": " + e.Message
}

// Is reports whether target is the sentinel for e's kind.
func (e *Error) Is(target error) bool {
	return target == e.sentinel()
}

func (e *Error) sentinel() error {
	switch e.Kind {
	case KindInvalidArgument:
		return ErrInvalidArgument
	case KindParse:
		return ErrParse
	case KindUnsupported:
		return ErrUnsupported
	default:
		return ErrInternal
	}
}
