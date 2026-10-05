package sqlscope

import "strconv"

// Option configures a sqlscope call.
type Option func(*options)

type options struct {
	Dialect       string              `json:"dialect,omitempty"`
	Schema        map[string][]string `json:"schema,omitempty"`
	TableNames    []string            `json:"tableNames,omitempty"`
	TablePatterns []string            `json:"tablePatterns,omitempty"`
	DefaultDB     string              `json:"defaultDb,omitempty"`
	StripCatalogs []string            `json:"stripCatalogs,omitempty"`

	// tableNamesSet distinguishes WithTableNames() with no names (an error)
	// from not restricting the scope at all.
	tableNamesSet bool
}

func (o *options) request() map[string]any {
	request := map[string]any{}
	if o.Dialect != "" {
		request["dialect"] = o.Dialect
	}
	if o.Schema != nil {
		request["schema"] = o.Schema
	}
	if o.tableNamesSet {
		names := o.TableNames
		if names == nil {
			names = []string{}
		}
		request["tableNames"] = names
	}
	if len(o.TablePatterns) > 0 {
		request["tablePatterns"] = o.TablePatterns
	}
	if o.DefaultDB != "" {
		request["defaultDb"] = o.DefaultDB
	}
	if len(o.StripCatalogs) > 0 {
		request["stripCatalogs"] = o.StripCatalogs
	}
	return request
}

// WithDialect selects the SQL dialect ("trino", "postgres", "mysql",
// "starrocks", "spark", "hive", "snowflake", ...). The default is trino.
func WithDialect(dialect string) Option {
	return func(o *options) { o.Dialect = dialect }
}

// WithSchema supplies table schemas: table name -> ordered column names. Keys
// may be bare, schema- or catalog-qualified. The schema expands wildcards and
// attributes unqualified columns. A table listed with no columns has zero
// columns; an absent table is unknown.
//
// Read by ColumnOrigins, OutputColumns, ReferencedColumns and ColumnUsages.
func WithSchema(schema map[string][]string) Option {
	return func(o *options) { o.Schema = schema }
}

// WithTableNames restricts ApplyRowFilter to the named tables: bare
// ("orders"), schema-qualified ("sales.orders") or catalog-qualified
// ("iceberg.sales.orders"), matched case-insensitively. Composes additively
// with WithTableRegexp.
func WithTableNames(names ...string) Option {
	return func(o *options) {
		o.TableNames = append(o.TableNames, names...)
		o.tableNamesSet = true
	}
}

// WithTableRegexp restricts ApplyRowFilter to tables whose bare or fully
// qualified name matches any of the regular expressions.
func WithTableRegexp(patterns ...string) Option {
	return func(o *options) { o.TablePatterns = append(o.TablePatterns, patterns...) }
}

// WithDefaultDB sets the schema used to resolve unqualified table references
// against schema-qualified WithTableNames entries (ApplyRowFilter).
func WithDefaultDB(db string) Option {
	return func(o *options) { o.DefaultDB = db }
}

// WithStripCatalogs makes RewriteTables treat the catalogs as transparent
// while matching: with WithStripCatalogs("hive"), hive.sales.orders matches
// the match key "sales.orders".
func WithStripCatalogs(catalogs ...string) Option {
	return func(o *options) { o.StripCatalogs = append(o.StripCatalogs, catalogs...) }
}

func collect(opts []Option) (*options, error) {
	cfg := &options{}
	for i, opt := range opts {
		if opt == nil {
			return nil, &Error{Kind: KindInvalidArgument, Message: "option " + strconv.Itoa(i) + " is nil"}
		}
		opt(cfg)
	}
	return cfg, nil
}
