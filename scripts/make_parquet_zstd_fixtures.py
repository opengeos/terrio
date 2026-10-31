"""Writes the GeoParquet files of `crates/terrio/tests/data/parquet_zstd`: the same table in Snappy
and in Zstandard with v1 and v2 data pages, and with each column compressed differently.

Usage:
    python scripts/make_parquet_zstd_fixtures.py
"""

import json
import struct

import pyarrow as pa
import pyarrow.parquet as pq

out = "crates/terrio/tests/data/parquet_zstd"
n=500
wkb=[struct.pack('<BIdd',1,1,float(i%50)-25,float(i%20)-10) for i in range(n)]
xs=[float(i%50)-25 for i in range(n)]; ys=[float(i%20)-10 for i in range(n)]
t=pa.table({
 'id':pa.array(range(n),pa.int64()),
 'name':pa.array([None if i%7==0 else 'category-%d'%(i%5) for i in range(n)]),
 'value':pa.array([None if i%11==0 else i*0.5 for i in range(n)],pa.float64()),
 'tags':pa.array([[f't{i%3}',f'u{i%2}'] if i%13 else None for i in range(n)],pa.list_(pa.string())),
 'bbox':pa.array([{'xmin':x,'ymin':y,'xmax':x,'ymax':y} for x,y in zip(xs,ys)],pa.struct([(k,pa.float64()) for k in('xmin','ymin','xmax','ymax')])),
 'geometry':pa.array(wkb,pa.binary()),
})
geo={'version':'1.1.0','primary_column':'geometry','columns':{'geometry':{'encoding':'WKB','geometry_types':['Point'],'covering':{'bbox':{k:['bbox',k] for k in('xmin','ymin','xmax','ymax')}}}}}
t=t.replace_schema_metadata({'geo':json.dumps(geo)})
w=dict(row_group_size=120,data_page_size=512,write_statistics=True)
pq.write_table(t,f'{out}/snappy.parquet',compression='snappy',**w)
pq.write_table(t,f'{out}/zstd_v1.parquet',compression='zstd',**w)
pq.write_table(t,f'{out}/zstd_v2.parquet',compression='zstd',data_page_version='2.0',**w)
pq.write_table(t,f'{out}/zstd_mixed.parquet',compression={'id':'zstd','name':'snappy','value':'gzip','tags.list.element':'zstd','bbox.xmin':'zstd','bbox.ymin':'none','bbox.xmax':'zstd','bbox.ymax':'lz4','geometry':'zstd'},**w)
