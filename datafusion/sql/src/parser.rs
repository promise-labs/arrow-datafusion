// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! DataFusion SQL Parser based on [`sqlparser`]

use datafusion_common::parsers::CompressionTypeVariant;

use sqlparser::{
    ast::{
        ColumnDef, ColumnOptionDef, HiveDistributionStyle, Ident, ObjectName,
        Statement as SQLStatement, TableConstraint,
    },
    dialect::{keywords::Keyword, Dialect, GenericDialect},
    parser::{Parser, ParserError},
    tokenizer::{Token, TokenWithLocation, Tokenizer},
};
use std::str::FromStr;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt, fs,
    path::{Path, PathBuf},
    process::exit,
};
extern crate regex;

use lazy_static::lazy_static;
use std::sync::Mutex;
// use crate::{dialect::Dialect, parser::{Parser, ParserError}, ast::Statement, tokenizer::Token, keywords::Keyword};

use once_cell::sync::OnceCell;

pub static VERBOSE_FLAG: OnceCell<i8> = OnceCell::new();

lazy_static! {
    /// collects all files that have been visited so far
    pub static ref VISITED_FILES: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    // collects all packages that have been visited so far
    pub static ref VISITED_CATALOGS: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    // collects all external table locations, catalog.schema.table -> relative
    pub static ref VISITED_SCHEMAS: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    // collects all external table locations, catalog.schema.table -> relative path
    pub static ref LOCATIONS: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
}
pub static CATALOG: &str = "catalog.yml";
pub static WORKSPACE: &str = "workspace.yml";

pub static DATA_DIR: &str = ".sdfcache";
pub const SOURCE_CACHE: &str = "source_cache.csv";
pub const DATA_CACHE: &str = "asset_cache.csv";

const DEFAULT_CATALOG: &str = "sdf";
const DEFAULT_SCHEMA: &str = "public";

pub fn visit(filename: &str, catalog: &str, schema: &str) {
    VISITED_FILES.lock().unwrap().insert(filename.to_owned());
    VISITED_CATALOGS.lock().unwrap().insert(catalog.to_owned());
    VISITED_SCHEMAS
        .lock()
        .unwrap()
        .insert(format!("{}.{}", catalog, schema));
}

// Removes directory path and returns the file name; like path.filename, but for strings
pub fn basename(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[i + 1..].to_owned(),
        None => path.to_owned(),
    }
}

// Removes basename from directory path
pub fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[0..i].to_owned(),
        None => "".to_owned(),
    }
}
pub fn extension(path: &str) -> String {
    match path.rfind('.') {
        Some(i) => path[i + 1..].to_owned(),
        None => "".to_owned(),
    }
}

pub fn strip_extension(path: &str, ext: &str) -> String {
    match path.strip_suffix(ext) {
        Some(base) => base.to_owned(),
        None => path.to_owned(),
    }
}

pub fn swap_extension(path: &str, old: &str, new: &str) -> String {
    match path.strip_suffix(old) {
        Some(base) => format!("{}{}", base, new),
        None => format!("{}{}", path, new),
    }
}

pub fn find_package_file(starting_directory: &Path) -> Option<PathBuf> {
    let mut path: PathBuf = starting_directory.into();
    let root_filename = Path::new(CATALOG);

    loop {
        path.push(root_filename);
        if path.is_file() {
            break Some(path.canonicalize().unwrap());
        }
        if !(path.pop() && path.pop()) {
            // remove file && remove parent
            break None;
        }
    }
}

pub fn find_package_path(starting_directory: &Path) -> Option<PathBuf> {
    if let Some(path) = find_package_file(Path::new(&starting_directory)) {
        let mut tmp: PathBuf = path.into();
        tmp.pop();
        Some(tmp)
    } else {
        None
    }
}

// HACK HACK: two problems: 1) this copies the code from build.rs of SDF; 
// 2) Datafusion should not be aware of the workspace file; it should support 
// a way of setting the root dir as a session parameter

pub fn find_file(starting_directory: &Path, file: &Path) -> Option<PathBuf> {
    let mut path: PathBuf = starting_directory.into();
    loop {
        path.push(file);
        if path.is_file() {
            break Some(path.to_path_buf().canonicalize().unwrap());
        }
        if !(path.pop() && path.pop()) {
            // remove file && remove parent
            break None;
        }
    }
}

pub fn find_path(starting_directory: &Path, file: &Path) -> Option<String> {
    if let Some(path) = find_file(Path::new(&starting_directory), file) {
        let mut tmp: PathBuf = path.into();
        tmp.pop();
        Some(tmp.display().to_string())
    } else {
        None
    }
}

fn find_workspace_dir(search_start: &str) -> Option<String> {
    let start = Path::new(search_start);
    find_path(start, &Path::new(WORKSPACE))
}

fn get_full_path(ws_dir: &str, input: &str) -> Option<String> {
    let input_path = Path::new(input);
    if input_path.is_absolute() {
        if let Some(p) = input_path.canonicalize().ok() {
            p.to_str().map(|x| x.to_owned())
        } else {
            None
        }
    } else {
        if let Some(p) = Path::new(ws_dir).join(input).canonicalize().ok() {
            p.to_str().map(|x| x.to_owned())
        } else {
            None
        }
    }
}

fn exists_full_path(path: &str, start_path: &str) -> bool {
    if let Some(ws_dir) = find_workspace_dir(start_path) {
        if let Some(full) = get_full_path(&ws_dir, path) {
            Path::new(&full).exists()
        } else {
            false
        }
    } else {
        false
    }
}

// Use `Parser::expected` instead, if possible
macro_rules! parser_err {
    ($MSG:expr) => {
        Err(ParserError::ParserError($MSG.to_string()))
    };
}

fn parse_file_type(s: &str) -> Result<String, ParserError> {
    // let res = FILENAME.lock().unwrap().replace(String::from("other"));
    Ok(s.to_uppercase())
}

/// DataFusion extension DDL for `CREATE EXTERNAL TABLE`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateExternalTable {
    /// Table name
    pub name: String,
    /// Optional schema
    pub columns: Vec<ColumnDef>,
    /// File type (Parquet, NDJSON, CSV, etc)
    pub file_type: String,
    /// CSV Header row?
    pub has_header: bool,
    /// User defined delimiter for CSVs
    pub delimiter: char,
    /// Path to file
    pub location: String,
    /// Partition Columns
    pub table_partition_cols: Vec<String>,
    /// Option to not error if table already exists
    pub if_not_exists: bool,
    /// File compression type (GZIP, BZIP2, XZ)
    pub file_compression_type: CompressionTypeVariant,
    /// Table(provider) specific options
    pub options: HashMap<String, String>,
}

impl fmt::Display for CreateExternalTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE EXTERNAL TABLE ")?;
        if self.if_not_exists {
            write!(f, "IF NOT EXISTS ")?;
        }
        write!(f, "{} ", self.name)?;
        write!(f, "STORED AS {} ", self.file_type)?;
        write!(f, "LOCATION {} ", self.location)
    }
}

/// DataFusion extension DDL for `DESCRIBE TABLE`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeTable {
    /// Table name
    pub table_name: ObjectName,
}

impl fmt::Display for DescribeTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.table_name)
    }
}

/// DataFusion Statement representations.
///
/// Tokens parsed by [`DFParser`] are converted into these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// ANSI SQL AST node with package_schema_path
    Statement(Box<SQLStatement>),
    /// Extension: `CREATE EXTERNAL TABLE` with package_path module_path
    CreateExternalTable(CreateExternalTable),
    /// Extension: `DESCRIBE TABLE` with package_path module_path
    DescribeTable(DescribeTable),
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Statement(s) => write!(f, "{}", s),
            Statement::CreateExternalTable(s) => write!(f, "{}", s),
            Statement::DescribeTable(s) => write!(f, "{}", s),
        }
    }
}

/// SDF StatementMeta
///
/// The location at which the statement is defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementMeta {
    pub catalog: String,
    pub schema: String,
    pub table: String,
    pub line_number: i32,
    pub filename: String,
}

impl fmt::Display for StatementMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}.{}{}",
            self.filename,
            self.line_number,
            self.catalog,
            self.schema,
            if self.table == "" {
                "".to_owned()
            } else {
                ".".to_owned() + &self.table
            },
        )
    }
}

impl StatementMeta {
    /// An empty statement definition location
    pub fn empty() -> Self {
        StatementMeta {
            catalog: String::new(),
            schema: String::new(),
            table: String::new(),
            line_number: 0,
            filename: String::new(),
        }
    }
    /// An statement definition location without line number
    //   That's' what Datafusion gives us today
    pub fn new(catalog: String, schema: String) -> Self {
        StatementMeta {
            catalog,
            schema,
            table: String::new(),
            line_number: 0,
            filename: String::new(),
        }
    }
    /// An statement definition location without line number
    pub fn new_with_table(
        catalog: String,
        schema: String,
        table: String,
        filename: String,
    ) -> Self {
        StatementMeta {
            catalog,
            schema,
            table,
            line_number: 0,
            filename,
        }
    }
    /// Return schema_file name, which is relative to workspace
    pub fn schema_filename(&self) -> String {
        format!("{},{}.sql", self.catalog, self.schema)
    }
}

/// DataFusion SQL Parser based on [`sqlparser`]
///
/// This parser handles DataFusion specific statements, delegating to
/// [`Parser`](sqlparser::parser::Parser) for other SQL statements.
#[allow(dead_code)]
pub struct DFParser<'a> {
    parser: Parser<'a>,
    catalog: String,
    schema: String,
    table: String,
    filename: String,
}

impl<'a> DFParser<'a> {
    /// Create a new parser for the specified tokens using the
    /// [`GenericDialect`].
    pub fn new(sql: &str) -> Result<Self, ParserError> {
        let dialect = &GenericDialect {};
        DFParser::new_with_dialect(sql, dialect)
    }

    /// Create a new parser for the specified tokens with the
    /// specified dialect.
    pub fn new_with_dialect(
        sql: &str,
        dialect: &'a dyn Dialect,
    ) -> Result<Self, ParserError> {
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize_with_location()?;

        Ok(DFParser {
            parser: Parser::new(dialect).with_tokens_with_locations(tokens),
            catalog: String::new(),
            schema: String::new(),
            table: String::new(),
            filename: String::new(),
        })
    }

    pub fn new_with_dialect_and_scope(
        sql: &str,
        dialect: &'a dyn Dialect,
        filename: String,
        catalog: String,
        schema: String,
        table: String,
    ) -> Result<Self, ParserError> {
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize_with_location()?;
        Ok(DFParser {
            parser: Parser::new(dialect).with_tokens_with_locations(tokens), // filename
            catalog,
            schema,
            table,
            filename,
        })
    }

    /// Parse a sql string into one or [`Statement`]s using the
    /// [`GenericDialect`].
    pub fn parse_sql(sql: &str) -> Result<VecDeque<Statement>, ParserError> {
        let dialect = &GenericDialect {};
        DFParser::parse_sql_with_dialect(sql, dialect)
    }

    /// Parse a SQL statement and produce a set of statements with dialect
    pub fn parse_sql_with_scope(
        sql: &str,
        filename: String,
        catalog: String,
        schema: String,
        table: String,
    ) -> Result<VecDeque<(Statement, StatementMeta)>, ParserError> {
        let dialect = &GenericDialect {};
        DFParser::parse_sql_with_dialect_and_scope(
            sql, dialect, filename, catalog, schema, table,
        )
    }
    /// Parse a SQL statement and produce a set of statements
    pub fn parse_sql_with_dialect(
        sql: &str,
        dialect: &dyn Dialect,
    ) -> Result<VecDeque<Statement>, ParserError> {
        let parser = DFParser::new_with_dialect(sql, dialect)?;
        Self::parse_statements(parser)
            .map(|list| list.into_iter().map(|elem| elem.0).collect())
    }

    /// Parse a SQL statement and produce a set of statements inside a given scope
    pub fn parse_sql_with_dialect_and_scope(
        sql: &str,
        dialect: &dyn Dialect,

        filename: String,
        catalog: String,
        schema: String,
        table: String,
    ) -> Result<VecDeque<(Statement, StatementMeta)>, ParserError> {
        let parser = DFParser::new_with_dialect_and_scope(
            sql,
            dialect,
            filename.to_owned(),
            catalog,
            schema,
            table,
        )?;
        match Self::parse_statements(parser) {
            Ok(res) => Ok(res),
            Err(sqlparser::parser::ParserError::ParserError(err)) => Err(
                ParserError::ParserError(format!("'{}': {}", &filename, err.to_string())),
            ),
            Err(sqlparser::parser::ParserError::TokenizerError(err)) => Err(
                ParserError::ParserError(format!("'{}': {}", &filename, err.to_string())),
            ),
            Err(sqlparser::parser::ParserError::RecursionLimitExceeded) => {
                Err(ParserError::ParserError(format!(
                    "'{}': {}",
                    &filename, "Recursion Limit Exceeded!"
                )))
            }
        }
    }

    fn parse_statements(
        mut parser: DFParser,
    ) -> Result<VecDeque<(Statement, StatementMeta)>, ParserError> {
        let mut stmts: VecDeque<(Statement, StatementMeta)> = VecDeque::new();
        let mut expecting_statement_delimiter = false;
        loop {
            // ignore empty statements (between successive statement delimiters)
            while parser.parser.consume_token(&Token::SemiColon) {
                expecting_statement_delimiter = false;
            }
            if parser.parser.peek_token() == Token::EOF {
                break;
            }
            if expecting_statement_delimiter {
                return parser.expected("End of statement", parser.parser.peek_token());
            }
            let expected_token = parser.parser.next_token();
            let result_statements = match expected_token.token.to_owned() {
                Token::Word(w) => match w.keyword {
                    Keyword::USE => Self::parse_use(&mut parser),
                    _ => {
                        parser.parser.prev_token();
                        parser.parse_statement().map(|op| VecDeque::from([op]))
                    }
                },
                _unexpected => parser.expected("End of statement", expected_token),
            };
            match result_statements {
                Ok(stms) => stmts.extend(stms),
                Err(err) => return Err(err),
            }

            expecting_statement_delimiter = true;
        }
        Ok(stmts)
    }

    /// Report an unexpected token
    fn expected<T>(
        &self,
        expected: &str,
        found: TokenWithLocation,
    ) -> Result<T, ParserError> {
        parser_err!(format!("Expected {expected}, found: {found}"))
    }

    /// Parse a file of SQL statements and produce an Abstract Syntax Tree (AST)
    pub fn parse_sql_file(
        dialect: &dyn Dialect,
        filename: String,
        catalog: String,
        schema: String,
        table: String,
        prefix: String,
    ) -> Result<VecDeque<(Statement, StatementMeta)>, ParserError> {
        let contents = fs::read_to_string(&filename)
            .unwrap_or_else(|_| panic!("Unable to read the file {}", &filename));
        let contents_with_prefix = prefix.clone() + &contents;

        let dialect: &dyn Dialect = &*dialect;
        let sql: &str = &contents_with_prefix;
        let parser = DFParser::new_with_dialect_and_scope(
            sql,
            dialect,
            filename.to_owned(),
            catalog,
            schema,
            table,
        )?;
        match Self::parse_statements(parser) {
            Ok(res) => Ok(res),
            Err(err) => {
                error!("{}: {}", &filename, err);
                exit(1)
            }
        }
    }

    fn parse_use(
        parser: &mut DFParser,
    ) -> Result<VecDeque<(Statement, StatementMeta)>, ParserError> {
        let next = parser.parser.next_token();
        match next.clone().token {
            Token::Word(w) => {
                // switch to a possibly new catalog

                //parse
                let catalog = w.value.clone();
                let _ = parser.parser.expect_token(&Token::Period);
                let schema = match parser.parser.parse_identifier() {
                    Ok(id) => id.value,
                    Err(_) => "".to_owned(),
                };
                let _ = parser.parser.expect_token(&Token::Period);
                let table = match parser.parser.parse_identifier() {
                    Ok(id) => id.value,
                    Err(_) => "".to_owned(),
                };
                // check catalog/schema naming
                // let regex = Regex::new(r"^[a-z0-9_]*$").unwrap();
                // if !regex.is_match(&catalog)
                //     || !regex.is_match(&schema)
                //     || !regex.is_match(&table)
                // {
                //     return parser.wrong_use(&format!("Catalog, schema, and table names must only be lowercase, digits or '_', found {}",catalog), next );
                // }
                println!(
                    "parsed {catalog}|{schema}|{table} |||| passed {}",
                    parser.catalog
                );

                if parser.catalog == "" {
                    println!(
                        "Source not under workspace -- skipping 'use {}.{}.{}' statement",
                        catalog, schema, table
                    );
                    return Ok(VecDeque::new());
                }

                // check whether new catalog exists
                let schema_filename = format!("{}/{}.sql", catalog, schema);
                let table_filename = format!("{}/{}/{}.sql", catalog, schema, table);

                println!(
                    "table_filename = {table_filename}|{}",
                    Path::new(&table_filename).is_file()
                );

                println!(
                    "schema_filename = {schema_filename}|{}",
                    Path::new(&schema_filename).is_file()
                );

                let (is_table, filename) = if Path::new(&table_filename).is_file() {
                    (true, table_filename)
                } else if Path::new(&schema_filename).is_file() {
                    (false, schema_filename)
                } else {
                    return Err(ParserError::ParserError(
                        format!(
                            "Missing schema file {} or table file {} ",
                            schema_filename, table_filename
                        )
                        .to_owned(),
                    ));
                };
                println!(
                    "Is table {}, filename {}, Visitedfiles {:?}",
                    is_table,
                    filename,
                    VISITED_FILES.lock().unwrap()
                );
                info!("-- USE {}.{}.{} from {}", catalog, schema, table, filename);
                // avoid duplicate uses
                if VISITED_FILES.lock().unwrap().contains(&filename) {
                    return Ok(VecDeque::new());
                }
                VISITED_FILES.lock().unwrap().insert(filename.to_owned());

                // create scopes
                let mut created_catalog = String::new();
                let mut created_schema = String::new();
                if !VISITED_CATALOGS.lock().unwrap().contains(&catalog) {
                    VISITED_CATALOGS.lock().unwrap().insert(catalog.to_owned());
                    created_catalog = format!("CREATE DATABASE {};\n", &catalog);
                    info!("{}", created_catalog);
                };
                let schema_id = format!("{}.{}", catalog, schema);
                if !VISITED_SCHEMAS.lock().unwrap().contains(&schema_id) {
                    VISITED_SCHEMAS.lock().unwrap().insert(schema_id);
                    created_schema = format!("CREATE SCHEMA {}.{};\n", &catalog, &schema);
                    info!("{}", created_schema);
                };

                info!("parsing: {}", filename);

                // continue parsing
                Self::parse_sql_file(
                    &GenericDialect {},
                    filename,
                    catalog,
                    schema,
                    if is_table { table } else { String::new() },
                    created_catalog + &created_schema,
                )
            }
            _unexpected => parser.expected("Object identifier", next)?,
        }
        // }
    }
    /// Parse a new expression
    pub fn parse_statement(&mut self) -> Result<(Statement, StatementMeta), ParserError> {
        match self.parser.peek_token().token {
            Token::Word(w) => {
                match w.keyword {
                    Keyword::CREATE => {
                        // move one token forward
                        self.parser.next_token();
                        // use custom parsing
                        self.parse_create()
                    }
                    Keyword::DESCRIBE => {
                        // move one token forward
                        self.parser.next_token();
                        // use custom parsing
                        self.parse_describe()
                    }
                    Keyword::SELECT | Keyword::WITH | Keyword::VALUES => {
                        // self.parser.prev_token();
                        let base_query = self.parser.parse_query()?;
                        let boxed_query = Box::new(base_query.to_owned());
                        if self.filename != "" {
                            // this is a select of of table definition
                            // let c = Ident::new(&self.catalog);
                            // let s = Ident::new(&self.schema);
                            let c = Ident::new(self.catalog.to_owned());
                            let s = Ident::new(self.schema.to_owned());
                            // let t = if self.table != "" {
                            //     Ident::new(&self.table)
                            // } else {
                            let t = Ident::new(strip_extension(
                                &basename(&self.filename),
                                ".sql",
                            ));
                            // };
                            let create_table_statement =
                                sqlparser::ast::Statement::CreateTable {
                                    or_replace: false,
                                    temporary: false,
                                    external: false,
                                    global: None,
                                    if_not_exists: false,
                                    /// Table name
                                    name: ObjectName(vec![c, s, t]), // vec of ident
                                    // name: ObjectName(vec![t]), // vec of ident
                                    /// Optional schema
                                    columns: vec![],
                                    constraints: vec![],
                                    hive_distribution: HiveDistributionStyle::NONE,
                                    hive_formats: None,
                                    table_properties: vec![],
                                    with_options: vec![],
                                    file_format: None,
                                    location: None,
                                    query: Some(boxed_query),
                                    without_rowid: false,
                                    like: None,
                                    clone: None,
                                    engine: None,
                                    default_charset: None,
                                    collation: None,
                                    on_commit: None,
                                    /// Click house "ON CLUSTER" clause:
                                    /// <https://clickhouse.com/docs/en/sql-reference/distributed-ddl/>
                                    on_cluster: None,
                                };
                            Ok((
                                Statement::Statement(Box::from(create_table_statement)),
                                self.with_meta("".to_owned()),
                            ))
                        } else {
                            // a usual select
                            let query_statement =
                                sqlparser::ast::Statement::Query(boxed_query);
                            Ok((
                                Statement::Statement(Box::from(query_statement)),
                                self.with_meta("".to_owned()),
                            ))
                        }
                    }
                    _ => {
                        let stm = self.parser.parse_statement()?;
                        Ok((
                            Statement::Statement(Box::from(stm)),
                            self.with_meta("".to_owned()),
                        ))
                    }
                }
            }
            _ => {
                // use the native parser
                let stm = self.parser.parse_statement()?;
                Ok((
                    Statement::Statement(Box::from(stm)),
                    self.with_meta("".to_owned()),
                ))
            }
        }
    }

    pub fn parse_describe(&mut self) -> Result<(Statement, StatementMeta), ParserError> {
        let table_name = self.parser.parse_object_name()?;
        let table_string = table_name.to_owned();
        let des = DescribeTable {
            table_name: table_name,
        };
        Ok((
            Statement::DescribeTable(des),
            self.with_meta(table_string.to_string()),
        ))
    }

    /// Parse a SQL CREATE statement
    pub fn parse_create(&mut self) -> Result<(Statement, StatementMeta), ParserError> {
        if self.parser.parse_keyword(Keyword::EXTERNAL) {
            self.parse_create_external_table()
        } else {
            let stm = self.parser.parse_create()?;
            let (name, qualified_stm) = match stm {
                SQLStatement::CreateView {
                    name,
                    cluster_by,
                    columns,
                    materialized,
                    or_replace,
                    query,
                    with_options,
                } => (
                    qualify_object_name(&self.catalog, &self.schema, &name),
                    SQLStatement::CreateView {
                        name: qualify_object_name(&self.catalog, &self.schema, &name),
                        cluster_by,
                        columns,
                        materialized,
                        or_replace,
                        query,
                        with_options,
                    },
                ),
                SQLStatement::CreateTable {
                    name,
                    collation,
                    columns,
                    constraints,
                    default_charset,
                    engine,
                    external,
                    file_format,
                    global,
                    hive_distribution,
                    hive_formats,
                    if_not_exists,
                    like,
                    location,
                    on_cluster,
                    on_commit,
                    or_replace,
                    query,
                    table_properties,
                    temporary,
                    with_options,
                    without_rowid,
                    clone,
                } => (
                    qualify_object_name(&self.catalog, &self.schema, &name),
                    SQLStatement::CreateTable {
                        name: qualify_object_name(&self.catalog, &self.schema, &name),
                        collation,
                        columns,
                        constraints,
                        default_charset,
                        engine,
                        external,
                        file_format,
                        global,
                        hive_distribution,
                        hive_formats,
                        if_not_exists,
                        like,
                        location,
                        on_cluster,
                        on_commit,
                        or_replace,
                        query,
                        table_properties,
                        temporary,
                        with_options,
                        without_rowid,
                        clone,
                    },
                ),
                SQLStatement::CreateVirtualTable {
                    name,
                    if_not_exists,
                    module_args,
                    module_name,
                } => (
                    qualify_object_name(&self.catalog, &self.schema, &name),
                    SQLStatement::CreateVirtualTable {
                        name: qualify_object_name(&self.catalog, &self.schema, &name),
                        if_not_exists,
                        module_args,
                        module_name,
                    },
                ),
                _ => (ObjectName(vec![]), stm),
            };

            let _table = match &qualified_stm {
                SQLStatement::CreateView { name, .. } => name.to_owned(),
                SQLStatement::CreateTable { name, .. }
                | SQLStatement::CreateVirtualTable { name, .. } => name.to_owned(),
                _ => sqlparser::ast::ObjectName(vec![]),
            };
            Ok((
                Statement::Statement(Box::from(qualified_stm)),
                self.with_meta_for_object_name(qualify_object_name(
                    &self.catalog,
                    &self.schema,
                    &name,
                )),
            ))
        }
    }

    fn with_meta(&mut self, table: String) -> StatementMeta {
        // TODO this should be the qualified name, where local schema catalog can ovveride default ones.
        // StatementMeta::new_with_table(
        //     self.catalog.to_owned(),
        //     self.schema.to_owned(),
        //     table,
        //     self.filename.to_owned(),
        // )
        let name: Vec<String> = table.split(".").map(|n| n.to_owned()).collect();
        match name.len() {
            0 => StatementMeta::new_with_table(
                DEFAULT_CATALOG.to_owned(),
                DEFAULT_SCHEMA.to_owned(),
                "N.N".to_owned(),
                self.filename.to_owned(),
            ),
            1 => StatementMeta::new_with_table(
                DEFAULT_CATALOG.to_owned(),
                DEFAULT_SCHEMA.to_owned(),
                name[0].to_owned(),
                self.filename.to_owned(),
            ),
            2 => StatementMeta::new_with_table(
                DEFAULT_CATALOG.to_owned(),
                name[0].to_owned(),
                name[1].to_owned(),
                self.filename.to_owned(),
            ),
            3 => StatementMeta::new_with_table(
                name[0].to_owned(),
                name[1].to_owned(),
                name[2].to_owned(),
                self.filename.to_owned(),
            ),
            _ => {
                eprintln!("with object {:?}", name);
                todo!("with object {:?}", name)
            }
        }
    }

    fn with_meta_for_object_name(&mut self, name: ObjectName) -> StatementMeta {
        // TODO this should be the qualified name, where local schema catalog can ovveride default ones.
        match name.0.len() {
            0 => StatementMeta::new_with_table(
                DEFAULT_CATALOG.to_owned(),
                DEFAULT_SCHEMA.to_owned(),
                "N.N".to_owned(),
                self.filename.to_owned(),
            ),
            1 => StatementMeta::new_with_table(
                DEFAULT_CATALOG.to_owned(),
                DEFAULT_SCHEMA.to_owned(),
                name.0[0].value.to_owned(),
                self.filename.to_owned(),
            ),
            2 => StatementMeta::new_with_table(
                DEFAULT_CATALOG.to_owned(),
                name.0[0].value.to_owned(),
                name.0[1].value.to_owned(),
                self.filename.to_owned(),
            ),
            3 => StatementMeta::new_with_table(
                name.0[0].value.to_owned(),
                name.0[1].value.to_owned(),
                name.0[2].value.to_owned(),
                self.filename.to_owned(),
            ),
            _ => {
                eprintln!("with object {}", name);
                todo!("with object {}", name)
            }
        }
    }

    fn parse_partitions(&mut self) -> Result<Vec<String>, ParserError> {
        let mut partitions: Vec<String> = vec![];
        if !self.parser.consume_token(&Token::LParen)
            || self.parser.consume_token(&Token::RParen)
        {
            return Ok(partitions);
        }

        loop {
            if let Token::Word(_) = self.parser.peek_token().token {
                let identifier = self.parser.parse_identifier()?;
                partitions.push(identifier.to_string());
            } else {
                return self.expected("partition name", self.parser.peek_token());
            }
            let comma = self.parser.consume_token(&Token::Comma);
            if self.parser.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected(
                    "',' or ')' after partition definition",
                    self.parser.peek_token(),
                );
            }
        }
        Ok(partitions)
    }

    // This is a copy of the equivalent implementation in sqlparser.
    fn parse_columns(
        &mut self,
    ) -> Result<(Vec<ColumnDef>, Vec<TableConstraint>), ParserError> {
        let mut columns = vec![];
        let mut constraints = vec![];
        if !self.parser.consume_token(&Token::LParen)
            || self.parser.consume_token(&Token::RParen)
        {
            return Ok((columns, constraints));
        }

        loop {
            if let Some(constraint) = self.parser.parse_optional_table_constraint()? {
                constraints.push(constraint);
            } else if let Token::Word(_) = self.parser.peek_token().token {
                let column_def = self.parse_column_def()?;
                columns.push(column_def);
            } else {
                return self.expected(
                    "column name or constraint definition",
                    self.parser.peek_token(),
                );
            }
            let comma = self.parser.consume_token(&Token::Comma);
            if self.parser.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected(
                    "',' or ')' after column definition",
                    self.parser.peek_token(),
                );
            }
        }

        Ok((columns, constraints))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, ParserError> {
        let name = self.parser.parse_identifier()?;
        let data_type = self.parser.parse_data_type()?;
        let collation = if self.parser.parse_keyword(Keyword::COLLATE) {
            Some(self.parser.parse_object_name()?)
        } else {
            None
        };
        let mut options = vec![];
        loop {
            if self.parser.parse_keyword(Keyword::CONSTRAINT) {
                let name = Some(self.parser.parse_identifier()?);
                if let Some(option) = self.parser.parse_optional_column_option()? {
                    options.push(ColumnOptionDef { name, option });
                } else {
                    return self.expected(
                        "constraint details after CONSTRAINT <name>",
                        self.parser.peek_token(),
                    );
                }
            } else if let Some(option) = self.parser.parse_optional_column_option()? {
                options.push(ColumnOptionDef { name: None, option });
            } else {
                break;
            };
        }
        Ok(ColumnDef {
            name,
            data_type,
            collation,
            options,
        })
    }

    fn parse_create_external_table(
        &mut self,
    ) -> Result<(Statement, StatementMeta), ParserError> {
        self.parser.expect_keyword(Keyword::TABLE)?;
        let if_not_exists =
            self.parser
                .parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let table_name = self.parser.parse_object_name()?;
        let (columns, _) = self.parse_columns()?;
        self.parser
            .expect_keywords(&[Keyword::STORED, Keyword::AS])?;

        // THIS is the main difference: we parse a different file format.
        let file_type = self.parse_file_format()?;

        let has_header = self.parse_csv_has_header();

        let has_delimiter = self.parse_has_delimiter();
        let delimiter = match has_delimiter {
            true => self.parse_delimiter()?,
            false => ',',
        };

        let file_compression_type = if self.parse_has_file_compression_type() {
            self.parse_file_compression_type()?
        } else {
            CompressionTypeVariant::UNCOMPRESSED
        };

        let table_partition_cols = if self.parse_has_partition() {
            self.parse_partitions()?
        } else {
            vec![]
        };

        let options = if self.parse_has_options() {
            self.parse_options()?
        } else {
            HashMap::new()
        };

        self.parser.expect_keyword(Keyword::LOCATION)?;
        let location = self.parser.parse_literal_string()?;
        if !location.starts_with("s3://") && !exists_full_path(&location, &self.filename)
        {
            return Err(ParserError::ParserError(format!(
                "Missing external file '{location}'"
            )));
        }
        let location2 = location.to_owned();
        let file_type2 = file_type.to_owned();
        let create = CreateExternalTable {
            name: qualify_name(&self.catalog, &self.schema, &table_name.to_string()),
            columns,
            file_type,
            has_header,
            delimiter,
            location,
            table_partition_cols,
            if_not_exists,
            file_compression_type,
            options,
        };

        LOCATIONS.lock().unwrap().insert(format!(
            "{}::{}",
            location2.to_ascii_lowercase(),
            file_type2.to_ascii_lowercase()
        ));

        Ok((
            Statement::CreateExternalTable(create),
            self.with_meta(
                qualify_name(&self.catalog, &self.schema, &table_name.to_string())
                    .to_owned(),
            ),
        ))
    }

    /// Parses the set of valid formats
    fn parse_file_format(&mut self) -> Result<String, ParserError> {
        let token = self.parser.next_token();
        match &token.token {
            Token::Word(w) => parse_file_type(&w.value),
            _ => self.expected("one of PARQUET, NDJSON, or CSV", token),
        }
    }

    /// Parses the set of
    fn parse_file_compression_type(
        &mut self,
    ) -> Result<CompressionTypeVariant, ParserError> {
        let token = self.parser.next_token();
        match &token.token {
            Token::Word(w) => CompressionTypeVariant::from_str(&w.value),
            _ => self.expected("one of GZIP, BZIP2, XZ", token),
        }
    }

    fn parse_has_options(&mut self) -> bool {
        self.parser.parse_keyword(Keyword::OPTIONS)
    }

    //
    fn parse_options(&mut self) -> Result<HashMap<String, String>, ParserError> {
        let mut options: HashMap<String, String> = HashMap::new();
        self.parser.expect_token(&Token::LParen)?;

        loop {
            let key = self.parser.parse_literal_string()?;
            let value = self.parser.parse_literal_string()?;
            options.insert(key.to_string(), value.to_string());
            let comma = self.parser.consume_token(&Token::Comma);
            if self.parser.consume_token(&Token::RParen) {
                // allow a trailing comma, even though it's not in standard
                break;
            } else if !comma {
                return self.expected(
                    "',' or ')' after option definition",
                    self.parser.peek_token(),
                );
            }
        }
        Ok(options)
    }

    fn parse_has_file_compression_type(&mut self) -> bool {
        self.parser
            .parse_keywords(&[Keyword::COMPRESSION, Keyword::TYPE])
    }

    fn parse_csv_has_header(&mut self) -> bool {
        self.parser
            .parse_keywords(&[Keyword::WITH, Keyword::HEADER, Keyword::ROW])
    }

    fn parse_has_delimiter(&mut self) -> bool {
        self.parser.parse_keyword(Keyword::DELIMITER)
    }

    fn parse_delimiter(&mut self) -> Result<char, ParserError> {
        let token = self.parser.parse_literal_string()?;
        match token.len() {
            1 => Ok(token.chars().next().unwrap()),
            _ => Err(ParserError::TokenizerError(
                "Delimiter must be a single char".to_string(),
            )),
        }
    }

    fn parse_has_partition(&mut self) -> bool {
        self.parser
            .parse_keywords(&[Keyword::PARTITIONED, Keyword::BY])
    }
}

/// todo
pub fn qualify_name(_catalog: &str, _schema: &str, name: &str) -> String {
    // let trimmed = name.trim_matches('_');
    // let c: Vec<&str> = name.split(".").collect();
    // let res = match c.len() {
    //     1 => format!("{}.{}.{}", catalog, schema, c[0]),
    //     2 => format!("{}.{}.{}", catalog, c[0], c[1]),
    //     3 => trimmed.to_owned(),
    //     _ => panic!(),
    // };
    // // println!("qualified_name {} {} {} => {}", catalog, schema, name, res);
    // res
    name.to_owned()
}

/// todo
pub fn qualify_object_name(
    _catalog: &str,
    _schema: &str,
    name: &ObjectName,
) -> ObjectName {
    // let c: Vec<Ident> = name.0.to_vec();
    // let res = match c.len() {
    //     1 => ObjectName(vec![
    //         Ident::new(catalog),
    //         Ident::new(schema),
    //         c[0].to_owned(),
    //     ]),
    //     2 => ObjectName(vec![Ident::new(catalog), c[0].to_owned(), c[1].to_owned()]),
    //     3 => name.to_owned(),
    //     _ => panic!(),
    // };
    // // println!("qualified_name {} {} {} => {}", catalog, schema, name, res);
    // res
    name.to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{DataType, Ident};
    use CompressionTypeVariant::UNCOMPRESSED;

    fn expect_parse_ok(sql: &str, expected: Statement) -> Result<(), ParserError> {
        let statements = DFParser::parse_sql(sql)?;
        assert_eq!(
            statements.len(),
            1,
            "Expected to parse exactly one statement"
        );
        assert_eq!(statements[0], expected);
        Ok(())
    }

    /// Parses sql and asserts that the expected error message was found
    fn expect_parse_error(sql: &str, expected_error: &str) {
        match DFParser::parse_sql(sql) {
            Ok(statements) => {
                panic!(
                    "Expected parse error for '{sql}', but was successful: {statements:?}"
                );
            }
            Err(e) => {
                let error_message = e.to_string();
                assert!(
                    error_message.contains(expected_error),
                    "Expected error '{expected_error}' not found in actual error '{error_message}'"
                );
            }
        }
    }

    fn make_column_def(name: impl Into<String>, data_type: DataType) -> ColumnDef {
        ColumnDef {
            name: Ident {
                value: name.into(),
                quote_style: None,
            },
            data_type,
            collation: None,
            options: vec![],
        }
    }

    #[test]
    fn create_external_table() -> Result<(), ParserError> {
        // positive case
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![make_column_def("c1", DataType::Int(display))],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.csv".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case with delimiter
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV DELIMITER '|' LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![make_column_def("c1", DataType::Int(display))],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: '|',
            location: "foo.csv".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: partitioned by
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV PARTITIONED BY (p1, p2) LOCATION 'foo.csv'";
        let display = None;
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![make_column_def("c1", DataType::Int(display))],
            file_type: "CSV".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.csv".into(),
            table_partition_cols: vec!["p1".to_string(), "p2".to_string()],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for case insensitive sql stmt with `WITH HEADER ROW` tokens
        let sqls = vec![
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV WITH HEADER ROW LOCATION 'foo.csv'",
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV with header row LOCATION 'foo.csv'"
        ];
        for sql in sqls {
            let expected = Statement::CreateExternalTable(CreateExternalTable {
                name: "t".into(),
                columns: vec![make_column_def("c1", DataType::Int(display))],
                file_type: "CSV".to_string(),
                has_header: true,
                delimiter: ',',
                location: "foo.csv".into(),
                table_partition_cols: vec![],
                if_not_exists: false,
                file_compression_type: UNCOMPRESSED,
                options: HashMap::new(),
            });
            expect_parse_ok(sql, expected)?;
        }

        // positive case: it is ok for sql stmt with `COMPRESSION TYPE GZIP` tokens
        let sqls = vec![
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE GZIP LOCATION 'foo.csv'", "GZIP"),
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE BZIP2 LOCATION 'foo.csv'", "BZIP2"),
            ("CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV COMPRESSION TYPE XZ LOCATION 'foo.csv'", "XZ"),
        ];
        for (sql, file_compression_type) in sqls {
            let expected = Statement::CreateExternalTable(CreateExternalTable {
                name: "t".into(),
                columns: vec![make_column_def("c1", DataType::Int(display))],
                file_type: "CSV".to_string(),
                has_header: false,
                delimiter: ',',
                location: "foo.csv".into(),
                table_partition_cols: vec![],
                if_not_exists: false,
                file_compression_type: CompressionTypeVariant::from_str(
                    file_compression_type,
                )?,
                options: HashMap::new(),
            });
            expect_parse_ok(sql, expected)?;
        }

        // positive case: it is ok for parquet files not to have columns specified
        let sql = "CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION 'foo.parquet'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "PARQUET".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.parquet".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for parquet files to be other than upper case
        let sql = "CREATE EXTERNAL TABLE t STORED AS parqueT LOCATION 'foo.parquet'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "PARQUET".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.parquet".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for avro files not to have columns specified
        let sql = "CREATE EXTERNAL TABLE t STORED AS AVRO LOCATION 'foo.avro'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "AVRO".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.avro".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: it is ok for avro files not to have columns specified
        let sql =
            "CREATE EXTERNAL TABLE IF NOT EXISTS t STORED AS PARQUET LOCATION 'foo.parquet'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "PARQUET".to_string(),
            has_header: false,
            delimiter: ',',
            location: "foo.parquet".into(),
            table_partition_cols: vec![],
            if_not_exists: true,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::new(),
        });
        expect_parse_ok(sql, expected)?;

        // Error cases: partition column does not support type
        let sql =
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV PARTITIONED BY (p1 int) LOCATION 'foo.csv'";
        expect_parse_error(sql, "sql parser error: Expected ',' or ')' after partition definition, found: int");

        // positive case: additional options (one entry) can be specified
        let sql =
            "CREATE EXTERNAL TABLE t STORED AS x OPTIONS ('k1' 'v1') LOCATION 'blahblah'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "X".to_string(),
            has_header: false,
            delimiter: ',',
            location: "blahblah".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::from([("k1".into(), "v1".into())]),
        });
        expect_parse_ok(sql, expected)?;

        // positive case: additional options (multiple entries) can be specified
        let sql =
            "CREATE EXTERNAL TABLE t STORED AS x OPTIONS ('k1' 'v1', k2 v2) LOCATION 'blahblah'";
        let expected = Statement::CreateExternalTable(CreateExternalTable {
            name: "t".into(),
            columns: vec![],
            file_type: "X".to_string(),
            has_header: false,
            delimiter: ',',
            location: "blahblah".into(),
            table_partition_cols: vec![],
            if_not_exists: false,
            file_compression_type: UNCOMPRESSED,
            options: HashMap::from([
                ("k1".into(), "v1".into()),
                ("k2".into(), "v2".into()),
            ]),
        });
        expect_parse_ok(sql, expected)?;

        // Error cases: partition column does not support type
        let sql =
            "CREATE EXTERNAL TABLE t STORED AS x OPTIONS ('k1' 'v1', k2 v2, k3) LOCATION 'blahblah'";
        expect_parse_error(sql, "sql parser error: Expected literal string, found: )");

        // Error case: `with header` is an invalid syntax
        let sql = "CREATE EXTERNAL TABLE t STORED AS CSV WITH HEADER LOCATION 'abc'";
        expect_parse_error(sql, "sql parser error: Expected LOCATION, found: WITH");

        // Error case: a single word `partitioned` is invalid
        let sql = "CREATE EXTERNAL TABLE t STORED AS CSV PARTITIONED LOCATION 'abc'";
        expect_parse_error(
            sql,
            "sql parser error: Expected LOCATION, found: PARTITIONED",
        );

        // Error case: a single word `compression` is invalid
        let sql = "CREATE EXTERNAL TABLE t STORED AS CSV COMPRESSION LOCATION 'abc'";
        expect_parse_error(
            sql,
            "sql parser error: Expected LOCATION, found: COMPRESSION",
        );

        Ok(())
    }

    #[test]
    fn invalid_compression_type() {
        let sql = "CREATE EXTERNAL TABLE t STORED AS CSV COMPRESSION TYPE ZZZ LOCATION 'blahblah'";
        expect_parse_error(
            sql,
            "sql parser error: Unsupported file compression type ZZZ",
        )
    }
}
