// Behavioral reference only; no JTS or GEOS implementation is embedded here.
// Compile/run against org.locationtech.jts:jts-core:1.20.0.
import java.io.*;
import java.util.*;
import org.locationtech.jts.geom.*;
import org.locationtech.jts.io.*;
import org.locationtech.jts.simplify.TopologyPreservingSimplifier;
import org.locationtech.jts.algorithm.hull.*;
import org.locationtech.jts.operation.valid.IsValidOp;
import org.locationtech.jts.index.strtree.*;

public class JtsOracle extends STRtree {
    List<Integer> packing(Geometry g) {
        List<ItemBoundable> items = new ArrayList<>();
        for (int i=0;i<g.getNumGeometries();i++)
            items.add(new ItemBoundable(g.getGeometryN(i).getEnvelopeInternal(),i));
        List<Integer> result = new ArrayList<>();
        if (items.isEmpty()) return result;
        for (Object node:createParentBoundables(items,0))
            for (Object item:((AbstractNode)node).getChildBoundables())
                result.add((Integer)((ItemBoundable)item).getItem());
        return result;
    }
    public static void main(String[] args) throws Exception {
        if (args.length==1 && args[0].equals("--version")) {
            System.out.println(org.locationtech.jts.JTSVersion.CURRENT_VERSION);return;
        }
        BufferedReader input=new BufferedReader(new InputStreamReader(System.in));
        String line;
        WKBReader reader=new WKBReader();
        WKBWriter writer=new WKBWriter(2,1);
        while ((line=input.readLine())!=null) {
            try {
                String[] p=line.split("\t");
                Geometry g=reader.read(WKBReader.hexToBytes(p[4]));
                double ratio=Double.parseDouble(p[1]);
                boolean tight=Boolean.parseBoolean(p[2]),holes=Boolean.parseBoolean(p[3]);
                if (p[0].equals("valid")) {System.out.println(new IsValidOp(g).isValid());continue;}
                if (p[0].equals("strtree")) {System.out.println(new JtsOracle().packing(g));continue;}
                Geometry result;
                switch (p[0]) {
                    case "simplify":result=TopologyPreservingSimplifier.simplify(g,ratio);break;
                    case "convex":result=g.convexHull();break;
                    case "concave":result=ConcaveHull.concaveHullByLengthRatio(g,ratio,holes);break;
                    case "polygon_hull":
                        ConcaveHullOfPolygons h=new ConcaveHullOfPolygons(g);
                        h.setMaximumEdgeLengthRatio(ratio);h.setTight(tight);h.setHolesAllowed(holes);
                        result=h.getHull();break;
                    default:throw new IllegalArgumentException("unknown operation");
                }
                System.out.println(WKBWriter.toHex(writer.write(result)));
            } catch (Exception e) { System.out.println("ERROR "+e.getClass().getSimpleName()+": "+e.getMessage()); }
        }
    }
}
