// Batch real-data API calls for the GDAL comparison script. Nothing is published.
import { readFileSync } from "node:fs";
import { basename } from "node:path";
import { createRequire } from "node:module";
const require=createRequire(import.meta.url);
const terrio=require(process.argv[2]);
const requests=JSON.parse(readFileSync(process.argv[3],"utf8"));
const names={check_geometry:"checkGeometry",make_valid:"makeValid"};
const camel=key=>key.replace(/_([a-z])/g,(_,c)=>c.toUpperCase());
const output=requests.map(({name,operation,source,options})=>{
  const input={bytes:new Uint8Array(readFileSync(source)),filename:basename(source)};
  const args=Object.fromEntries(Object.entries(options).map(([k,v])=>[camel(k),v]));
  const fn=terrio[names[operation]||operation];
  let result;
  if(operation==="buffer"){const distance=args.distance;delete args.distance;result=fn(input,distance,args);}
  else if(operation==="check_geometry"||operation==="make_valid")result=fn(input);
  else result=fn(input,args);
  return {name,geojson:result.toGeoJSON(),crs:result.crs};
});
process.stdout.write(JSON.stringify(output));
