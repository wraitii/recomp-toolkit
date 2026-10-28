
fn d9_output(input:vec4<f32>,vin:V2F)->vec4<f32> {
    var col=input;
    let a=clamp(col.a,0.0,1.0); let r=psp.alpha_ref;
    var alpha_pass=true;
    switch psp.alpha_func {case 1:{alpha_pass=false;} case 2:{alpha_pass=a<r;} case 3:{alpha_pass=a==r;} case 4:{alpha_pass=a<=r;} case 5:{alpha_pass=a>r;} case 6:{alpha_pass=a!=r;} case 7:{alpha_pass=a>=r;} default:{} }
    if (!alpha_pass) {discard;}
    var f=vin.fog;
    let z=abs(vin.fogz);
    switch psp.fog_mode {case 1:{f=exp(-psp.fog_density*z);} case 2:{let d=psp.fog_density*z;f=exp(-d*d);} case 3:{f=(psp.fog_end-z)/(psp.fog_end-psp.fog_start);} default:{} }
    if (psp.pad0!=0) {col=vec4<f32>(mix(psp.fog_color.rgb,col.rgb,clamp(f,0.0,1.0)),col.a);}
    if (psp.pad1!=0.0) {let c=clamp(col.rgb,vec3<f32>(0.0),vec3<f32>(1.0)); col=vec4<f32>(select(1.055*pow(c,vec3<f32>(1.0/2.4))-0.055,12.92*c,c<=vec3<f32>(0.0031308)),col.a);}
    return col;
}
