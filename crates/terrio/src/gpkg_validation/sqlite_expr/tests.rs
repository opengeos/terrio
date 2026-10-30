use super::*;

#[test]
fn sqlite_check_null_is_success_and_false_is_failure() {
    let ctx = Context { columns: &[], row: &[], rowid: None, table: None, encoding: 1, strict: false, unicode_case: false };
    assert!(Expression::parse("NULL").unwrap().check(&ctx).unwrap());
    assert!(!Expression::parse("0").unwrap().check(&ctx).unwrap());
    assert!(Expression::parse("NULL OR 1").unwrap().check(&ctx).unwrap());
    assert!(!Expression::parse("NULL AND 0").unwrap().check(&ctx).unwrap());
    assert!(Expression::parse("CASE WHEN 1 THEN 42 ELSE abs() END").is_err());
    assert!(Expression::parse("unknown_function(1)").is_err());
}
#[test]
fn ascii_and_unicode_case_profiles_are_explicit() {
    let mut ctx = Context { columns: &[], row: &[], rowid: None, table: None, encoding: 1, strict: false, unicode_case: false };
    assert_eq!(Expression::parse("lower('ÄA')").unwrap().evaluate(&ctx).unwrap(), Value::Text("Äa".into()));
    assert_eq!(Expression::parse("'Ä' LIKE 'ä'").unwrap().evaluate(&ctx).unwrap(), Value::Int(0));
    ctx.unicode_case = true;
    assert_eq!(Expression::parse("lower('ÄA')").unwrap().evaluate(&ctx).unwrap(), Value::Text("äa".into()));
    assert_eq!(Expression::parse("'Ä' LIKE 'ä'").unwrap().evaluate(&ctx).unwrap(), Value::Int(1));
}
fn decode(value: &serde_json::Value) -> Value {
    match value[0].as_str().unwrap() {
        "null" => Value::Null,
        "integer" => Value::Int(value[1].as_i64().unwrap()),
        "real" => Value::Real(f64::from_bits(value[1].as_u64().unwrap())),
        "text" => Value::Text(value[1].as_str().unwrap().into()),
        "blob" => {
            let s = value[1].as_str().unwrap();
            Value::Blob((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect())
        }
        _ => unreachable!(),
    }
}
#[test]
#[ignore = "requires SQLite Python bindings from the GDAL 3.13.3 reference environment"]
fn scalar_expressions_match_sqlite_values_and_types() {
    let script = r#"
import json,sqlite3,struct,itertools
def encode(v):
 if v is None:return ['null',None]
 if isinstance(v,int):return ['integer',v]
 if isinstance(v,float):return ['real',struct.unpack('<Q',struct.pack('<d',v))[0]]
 if isinstance(v,bytes):return ['blob',v.hex()]
 return ['text',v]
expressions=['NULL','TRUE','FALSE','"TRUE"','"missing"','[missing]','2 IS TRUE','2 IS +TRUE','NULL IS FALSE','NOT 1 = 1','NULL ISNULL','1 NOTNULL','NULL NOT NULL',
 '-9223372036854775808','-(9223372036854775808)','-((9223372036854775808))','-9223372036854775809','0xFFFFFFFFFFFFFFFF','0x8000000000000000','-0x8000000000000000','1_000+2',
 '9223372036854775807+1','-9223372036854775808/-1','-9223372036854775808%-1','0 AND abs(-9223372036854775808)','1 OR abs(-9223372036854775808)',
 "i='2'","+i='2'","r='2.0'","t='ab'","t COLLATE BINARY='ab'","t||''='ab'","(t||'' COLLATE NOCASE)='ab'","any='02'","typeof(any)",
 "i IN ('2')","i IN (NULL,3)","i NOT IN (NULL,3)",'NULL IN ()','NULL NOT IN ()','0 BETWEEN NULL AND -1','NULL BETWEEN 0 AND 1',
 "CASE i WHEN '2' THEN 'yes' ELSE 'no' END",'CASE WHEN NULL THEN 1 WHEN 2 THEN 3 ELSE 4 END','CASE WHEN 1 THEN 3 ELSE abs(-9223372036854775808) END',
 "nullif(t,'ab')","nullif(t,2)","coalesce(NULL,0,1)","ifnull(NULL,t)",'iif(0,1,2)','iif(0,1,1,2,3)','iif(0,1)',
 "min(2,'1')","max(2,'1')","min(t,'ab')","min('a','A' COLLATE NOCASE)",'max(1,NULL,2)',
 "abs('12')","abs('abc')",'abs(-9223372036854775808)',"typeof(abs('12'))","sign('1.5')","sign('bad')",
 "lower('ÄABC')","upper('äabc')","trim('xxabcxy','xy')","ltrim('xxabcxy','xy')","rtrim('xxabcxy','xy')",
 "instr('a'||char(0)||'b','b')","replace('a'||char(0)||'a','a','b')","length('a'||char(0)||'b')","octet_length('a'||char(0)||'b')",'length(b)','hex(b)',
 "unhex('4142')","unhex('4-1-42','-')","quote('a'||char(0)||'b')","quote(b)","unicode('水')",'char(65,0,66)',
 "json_valid('{}')","json_valid('bad')",'json_valid(NULL)',
 "'Ab' LIKE 'a_'","'Ä' LIKE 'ä'","'ab_c' LIKE 'ab!_c' ESCAPE '!'","'a' LIKE 'a' ESCAPE ''","'b' GLOB '[a-c]'","'b' GLOB '[^a]'","']' GLOB '[]]'","'x' GLOB '['",
 "sample.i=2",'rowid=1',"CAST('abc' AS INTEGER)","CAST('1e2' AS INTEGER)","CAST('1.0junk' AS NUMERIC)","CAST('1234567890123456.0' AS NUMERIC)","CAST('1.5' AS NUMERIC)","CAST(1 AS VARCHAR(20))",'CAST(1 AS NUMERIC(2.5))','CAST(1 AS VARCHAR())','CAST(1 AS VARCHAR(1,2,3))',
 'likelihood(i,0.5)','likelihood(i,1)','likelihood(i,r)',"upper('straße')","'ı' LIKE 'i'","'Σ' LIKE 'ς'",'CAST(1 AS VARCHAR(+-1))',"'a' GLOB 'a' ESCAPE '!'",
]
for left,right,op in itertools.product(['NULL','0','1','2',"'0'","'abc'","X'31'"],['NULL','0','1',"'1'"],['AND','OR','=','<>','<','IS','IS NOT','+','-','*','/','%','||','&','<<','>>']):expressions.append('('+left+') '+op+' ('+right+')')
for start in range(-8,9):
 for length in [None,-5,-1,0,1,3,9]:
  for value in ["'abcde'","X'0102030405'"]:
   expressions.append('substr('+value+','+str(start)+('' if length is None else ','+str(length))+')')
cases=[]
for strict in [False,True]:
 schema='CREATE TABLE sample(i INTEGER,r REAL,t TEXT COLLATE NOCASE,b BLOB,n '+('INTEGER' if strict else 'NUMERIC')+',any ANY)'+(' STRICT' if strict else '')
 with sqlite3.connect(':memory:') as db:
  db.execute(schema);db.execute('INSERT INTO sample VALUES(?,?,?,?,?,?)',(2,2.,'Ab',b'12','02','02'))
  row=[encode(v) for v in db.execute('SELECT * FROM sample').fetchone()]
  for expression in expressions:
   try:result=dict(value=encode(db.execute('SELECT ('+expression+') FROM sample').fetchone()[0]))
   except sqlite3.Error:result=dict(error=True)
   cases.append(dict(sql=expression,schema=schema,row=row,**result))
print(json.dumps(cases))
"#;
    let output = std::process::Command::new("python3").arg("-c").arg(script).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert!(cases.len() > 1000);
    let mut failures = Vec::new();
    for case in &cases {
        let sql = case["sql"].as_str().unwrap();
        let schema = Schema::parse(case["schema"].as_str().unwrap()).unwrap();
        let row: Vec<_> = case["row"].as_array().unwrap().iter().map(decode).collect();
        let ctx = Context {
            columns: &schema.columns,
            row: &row,
            rowid: Some(1),
            table: Some("sample"),
            encoding: 1,
            strict: schema.strict,
            unicode_case: true,
        };
        let actual = Expression::parse(sql).and_then(|expr| expr.evaluate(&ctx));
        let correct = if case["error"].as_bool() == Some(true) {
            actual.is_err()
        } else {
            let expected = decode(&case["value"]);
            match (&actual, &expected) {
                (Ok(Value::Real(a)), Value::Real(b)) => a.to_bits() == b.to_bits(),
                (Ok(value), _) => *value == expected,
                _ => false,
            }
        };
        if !correct {
            failures.push(format!(
                "strict={}: {sql}: actual={actual:?}, reference={}",
                schema.strict,
                case.get("value").unwrap_or(&case["error"])
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences of {} cases:\n{}",
        failures.len(),
        cases.len(),
        failures.into_iter().take(30).collect::<Vec<_>>().join("\n")
    );
    eprintln!("{} SQLite scalar-expression comparisons passed", cases.len());
}

#[test]
#[ignore = "requires GDAL 3.13.3 and real comparison data"]
fn check_and_generated_integrity_match_sqlite_on_real_derivatives() {
    let directory = tempfile::tempdir().unwrap();
    let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
    let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
gdal.UseExceptions()
root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
source=gdal.OpenEx(str(data/'countries.gpkg'),gdal.OF_VECTOR);name=source.GetLayer(0).GetName().replace('"','""')
gdal.VectorTranslate(str(base),source,layerName='countries',SQLStatement='SELECT * FROM "'+name+'" LIMIT 3',SQLDialect='SQLITE',layerCreationOptions=['SPATIAL_INDEX=NO']).Close();source=None
def add(name,sql):
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(base,path)
 with sqlite3.connect(path) as db:db.executescript(sql)
 with sqlite3.connect(path) as db:
  try:valid=db.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
  except sqlite3.Error:valid=False
 cases.append(dict(name=name,path=str(path),valid=valid))
for check,value in [('v>=0','2'),('v>=0','-1'),('v>0','NULL'),("typeof(v)='integer'",'1'),("typeof(v)='text'","'one'"),('v IN (0,1,2)','3'),('v NOT IN (0,NULL)','3'),('v BETWEEN 0 AND 10','5'),('v BETWEEN 0 AND 10','20'),('CASE WHEN v IS NULL THEN 1 ELSE v>0 END','NULL'),('v IS TRUE','2'),('abs(v)<10','-5'),('CAST(v AS INTEGER)>0',"'1tail'"),("length(v)<3","'long'"),("v LIKE 'a%'","'Ab'"),("json_valid(v)","'{}'"),("json_valid(v)","'invalid'")]:
 add(check+' '+value,'CREATE TABLE sample(v CHECK('+check+'));PRAGMA ignore_check_constraints=ON;INSERT INTO sample VALUES('+value+')')
add('named column and table checks','CREATE TABLE sample(a INTEGER CONSTRAINT positive CHECK(a>=0),b TEXT,CONSTRAINT short CHECK(length(b)<5));PRAGMA ignore_check_constraints=ON;INSERT INTO sample VALUES(-1,\'longer\')')
add('generated text affinity','CREATE TABLE sample(a INTEGER,b TEXT AS (a*2),CHECK(typeof(b)=\'text\'));INSERT INTO sample(a) VALUES(3)')
add('generated numeric affinity','CREATE TABLE sample(a TEXT,b INTEGER AS (a),CHECK(typeof(b)=\'integer\'));INSERT INTO sample(a) VALUES(\'03\')')
add('generated dependency order','CREATE TABLE sample(a INTEGER,c INTEGER AS (b+1),b INTEGER AS (a+1),CHECK(c=a+2));INSERT INTO sample(a) VALUES(3)')
add('generated violating CHECK','CREATE TABLE sample(a INTEGER,b INTEGER AS (a+1),CHECK(b<4));PRAGMA ignore_check_constraints=ON;INSERT INTO sample(a) VALUES(9)')
add('stored generated with CHECK','CREATE TABLE sample(a INTEGER,b INTEGER AS (a+1) STORED,CHECK(b<4));PRAGMA ignore_check_constraints=ON;INSERT INTO sample(a) VALUES(9)')
add('STRICT generated ANY retains text','CREATE TABLE sample(a TEXT,b ANY AS (a),CHECK(typeof(b)=\'text\')) STRICT;INSERT INTO sample(a) VALUES(\'03\')')
add('plain index over virtual column','CREATE TABLE sample(a INTEGER,b INTEGER AS (a+1));INSERT INTO sample(a) VALUES(1),(2);CREATE INDEX idx ON sample(b DESC)')
add('unique index over virtual column','CREATE TABLE sample(a INTEGER,b TEXT AS (a||\'x\') UNIQUE);INSERT INTO sample(a) VALUES(1),(2)')
add('stale index after virtual definition change',"CREATE TABLE sample(a INTEGER,b INTEGER AS (a+1));INSERT INTO sample(a) VALUES(1),(2);CREATE INDEX idx ON sample(b);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'a+1','a+2') WHERE name='sample'")
add('stored generated read semantics after definition change',"CREATE TABLE sample(a INTEGER,b INTEGER AS (a+1) STORED);INSERT INTO sample(a) VALUES(1),(2);CREATE INDEX idx ON sample(b);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'a+1','a+2') WHERE name='sample'")
add('missing CHECK column',"CREATE TABLE sample(a INTEGER CHECK(a>0));INSERT INTO sample VALUES(1);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql=replace(sql,'CHECK(a>0)','CHECK(missing>0)') WHERE name='sample'")
print(json.dumps(cases))
"#;
    let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert!(cases.len() > 25);
    for case in &cases {
        let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
        let db = Snapshot::read(&bytes).unwrap();
        let mut checker = Checker { db, report: CoreReport::default() };
        let result = checker.constraints(&bytes);
        let valid = result.is_ok() && checker.report.errors.is_empty();
        assert_eq!(valid, case["valid"].as_bool().unwrap(), "{}: {result:?}: {:?}", case["name"], checker.report.errors);
    }
    eprintln!("{} SQLite CHECK/generated comparison cases passed", cases.len());
}
