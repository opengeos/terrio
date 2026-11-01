// Real-data zonal binding calls for compare_zonal_bindings_with_gdal.py.
import {readFileSync,writeFileSync} from "node:fs";
import {basename} from "node:path";
import {createRequire} from "node:module";
const terrio=createRequire(import.meta.url)(process.argv[2]);
const requests=JSON.parse(readFileSync(process.argv[3],"utf8"));
const upload=path=>({filename:basename(path),bytes:new Uint8Array(readFileSync(path))});
const output=requests.map(({name,zones,rasters,options,operation,output})=>{
  try {
    const args=Object.fromEntries(Object.entries(options).map(([k,v])=>[k.replace(/_([a-z])/g,(_,c)=>c.toUpperCase()),v]));
    const result=operation==="singlepart" ? terrio.singlepart(upload(zones)) : terrio.zonalStats(upload(zones),rasters.map(upload),args);
    if(output)writeFileSync(output,result.toBytes("result.gpkg"));
    return {name,geojson:result.toGeoJSON(),crs:result.crs,output};
  } catch(error) {
    return {name,error:String(error)};
  }
});
process.stdout.write(JSON.stringify(output));
