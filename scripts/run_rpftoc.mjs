// Cross-read harness for named CADRG uploads through the real WebAssembly API.
import {readFileSync,writeFileSync} from "node:fs";
import {createRequire} from "node:module";
const require=createRequire(import.meta.url);
const terrio=require(process.argv[2]);
const request=JSON.parse(readFileSync(process.argv[3],"utf8"));
const frames=request.frames.map(({filename,path})=>({filename,bytes:new Uint8Array(readFileSync(path))}));
writeFileSync(request.output,terrio.rpftocCreate(frames,request.options));
