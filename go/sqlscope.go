package sqlscope

// CTEDef is a common table expression "Name AS (Query)". Name is an
// identifier value, not SQL text; it is quoted as needed.
type CTEDef struct {
	Name  string `json:"name"`
	Query string `json:"query"`
}

// TableRef is a table name, optionally schema- and catalog-qualified.
type TableRef struct {
	Catalog string `json:"catalog,omitempty"`
	Schema  string `json:"schema,omitempty"`
	Table   string `json:"table"`
}

// UnionRewrite is a derived table backed by UNION DISTINCT over Branches.
// TableAlias is used for references without an alias of their own.
type UnionRewrite struct {
	TableAlias string     `json:"tableAlias"`
	Columns    []string   `json:"columns"`
	Branches   []TableRef `json:"branches"`
}

// TableRewrite replaces references to MatchKey ("schema.table"). Set exactly
// one of Inline, which becomes (SELECT * FROM Inline), or Union.
type TableRewrite struct {
	MatchKey string        `json:"matchKey"`
	Inline   *TableRef     `json:"inline,omitempty"`
	Union    *UnionRewrite `json:"union,omitempty"`
}

// ColumnClause is the SQL clause that contains a column reference.
type ColumnClause string

const (
	ColumnClauseSelect          ColumnClause = "SELECT"
	ColumnClauseFrom            ColumnClause = "FROM"
	ColumnClauseJoinOn          ColumnClause = "JOIN_ON"
	ColumnClauseJoinUsing       ColumnClause = "JOIN_USING"
	ColumnClauseWhere           ColumnClause = "WHERE"
	ColumnClauseGroupBy         ColumnClause = "GROUP_BY"
	ColumnClauseHaving          ColumnClause = "HAVING"
	ColumnClauseQualify         ColumnClause = "QUALIFY"
	ColumnClauseWindow          ColumnClause = "WINDOW"
	ColumnClauseOrderBy         ColumnClause = "ORDER_BY"
	ColumnClauseSortBy          ColumnClause = "SORT_BY"
	ColumnClauseDistributeBy    ColumnClause = "DISTRIBUTE_BY"
	ColumnClauseClusterBy       ColumnClause = "CLUSTER_BY"
	ColumnClauseConnectBy       ColumnClause = "CONNECT_BY"
	ColumnClauseLateralView     ColumnClause = "LATERAL_VIEW"
	ColumnClauseUpdateSetTarget ColumnClause = "UPDATE_SET_TARGET"
	ColumnClauseUpdateSetValue  ColumnClause = "UPDATE_SET_VALUE"
	ColumnClauseMergeOn         ColumnClause = "MERGE_ON"
	ColumnClauseMergeWhen       ColumnClause = "MERGE_WHEN"
)

// ColumnUsage is one distinct use of a root table column in a clause.
type ColumnUsage struct {
	Table  string       `json:"table"`
	Column string       `json:"column"`
	Clause ColumnClause `json:"clause"`
}

func run[T any](operation string, fields map[string]any, opts []Option) (T, error) {
	var out T
	cfg, err := collect(opts)
	if err != nil {
		return out, err
	}
	fields["options"] = cfg.request()
	err = invoke(operation, fields, &out)
	return out, err
}

// ApplyRowFilter wraps every in-scope physical table of a query in a derived
// table filtered by predicate:
//
//	SELECT * FROM a  ->  SELECT * FROM (SELECT * FROM a WHERE <predicate>) AS a
//
// Filtering each table before it is joined keeps the filter correct across
// outer joins, CTEs, subqueries and set operations. The input must be one
// SELECT or set operation. predicate is parsed as one boolean expression and
// inserted into the AST; bind or escape its values before calling.
//
// By default every physical table is filtered; WithTableNames,
// WithTableRegexp and WithDefaultDB restrict the scope. CTE references,
// table functions and DUAL are never wrapped. Output is regenerated
// (normalized, comments dropped) and verified to parse. When no table is in
// scope the input is returned unchanged.
func ApplyRowFilter(sql, predicate string, opts ...Option) (string, error) {
	return run[string]("apply_row_filter", map[string]any{"sql": sql, "predicate": predicate}, opts)
}

// InjectCTEs adds CTE definitions to the root WITH clause of a query, before
// the CTEs it already declares. Definitions keep their order, so each may use
// earlier ones. A name that repeats another definition or an existing root
// CTE is rejected. An empty ctes slice returns sql unchanged.
func InjectCTEs(sql string, ctes []CTEDef, opts ...Option) (string, error) {
	if len(ctes) == 0 {
		return sql, nil
	}
	return run[string]("inject_ctes", map[string]any{"sql": sql, "ctes": ctes}, opts)
}

// RewriteTables replaces physical table references in a query according to
// rewrites. Matched references become derived tables that keep their alias,
// or their bare name when unaliased (disambiguated on collisions); qualified
// column and star references are rebound onto it. CTE references are never
// replaced. Catalogs given to WithStripCatalogs are transparent while
// matching. When nothing matches the input is returned unchanged.
func RewriteTables(sql string, rewrites []TableRewrite, opts ...Option) (string, error) {
	if len(rewrites) == 0 {
		return sql, nil
	}
	return run[string]("rewrite_tables", map[string]any{"sql": sql, "rewrites": rewrites}, opts)
}

// ColumnOrigins returns the source columns whose values flow into the
// statement's result, keyed by root physical table, with sorted columns.
// Filter-only positions (WHERE, JOIN ON, GROUP BY, HAVING, ORDER BY) and the
// right side of INTERSECT / EXCEPT are excluded. Every table read is present,
// possibly with no columns. UPDATE and MERGE report their assigned values.
func ColumnOrigins(sql string, opts ...Option) (map[string][]string, error) {
	return run[map[string][]string]("column_origins", map[string]any{"sql": sql}, opts)
}

// OutputColumns returns the names of the columns a statement outputs, in
// order. Unaliased expressions are named _col{i}; * expands from WithSchema
// or stays "*". An explicit column list on CREATE VIEW, CREATE TABLE or
// INSERT wins. It returns nil for statements without columns (DROP TABLE).
func OutputColumns(sql string, opts ...Option) ([]string, error) {
	return run[[]string]("output_columns", map[string]any{"sql": sql}, opts)
}

// ReferencedColumns returns the columns referenced anywhere in a statement,
// filters included, keyed by root physical table, with sorted columns. It is
// a superset of ColumnOrigins and also accepts DELETE, UPDATE and MERGE.
// References that cannot be resolved precisely are attributed to every
// candidate table rather than dropped.
func ReferencedColumns(sql string, opts ...Option) (map[string][]string, error) {
	return run[map[string][]string]("referenced_columns", map[string]any{"sql": sql}, opts)
}

// ColumnUsages returns every distinct (table, column, clause) use in a
// statement, sorted by table, column and clause.
func ColumnUsages(sql string, opts ...Option) ([]ColumnUsage, error) {
	usages, err := run[[]ColumnUsage]("column_usages", map[string]any{"sql": sql}, opts)
	if usages == nil && err == nil {
		usages = []ColumnUsage{}
	}
	return usages, err
}
