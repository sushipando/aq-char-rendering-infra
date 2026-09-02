 What was built — services/component-raster-rust/                                              
                                                                                               
 resvg as a library dependency — exactly as you remember from the plan's "Future Rust          
 component-raster worker" section:                                                             
 - usvg::Tree::from_data → resvg::render into a tiny_skia::Pixmap, pinned to 0.48.1 (the same  
   source the Python image's /opt/resvg/resvg CLI is built from), with the CLI's exact         
   demultiply rounding.                                                                        
 - Full port of the Python SVG pipeline onto a small namespace-safe DOM (import_ffdec_symbol:  
   zoom-wrapper removal, tint color rules, authored placement CXFORM feColorMatrix filters,    
   minimum-stroke calibration, font-zoom normalization, part_/placed_ id rewriting) +          
   tight-page assembly + in-process tar.gz extraction + PNG encode + the same result record /  
   profile logs / benchmark_output_prefix.                                                     
                                                                                               
 Downsampling — this is where your fast_image_resize idea hit a hard constraint: FIR's integer 
 Lanczos differs from Pillow by 1/255 at AA edges, which flipped an alpha row and moved the    
 recorded bbox by 1px (the harness caught exactly that). So the default downscaler in          
 resample.rs is a bit-exact port of Pillow's Resample.c (float Lanczos kernel, 2²² fixed-point 
 coefficients, 2²¹ bias, two-pass layout) plus the RGBa premultiply/unpremultiply round trips. 
 fast_image_resize stays wired behind AQW_DOWNSAMPLER=fast_image_resize for benchmarks.        
                                                                                               
 Parity evidence                                                                               
                                                                                               
 scripts/rust_raster_parity.py runs the Python worker (resvg 0.48.1 CLI + Pillow) and Rust     
 local-raster on identical stores and requires exact RGBA:                                     
 - synthetic zoom-2 job (tints, CXFORMs, stroke calibration, gradients, font groups, rotation) 
   at 2× and 1× downsampling → exact, 0 pixel diffs                                            
 - real FFDec pet export (filters/gradients/121 stroke markers) → exact (its frame 000020 is   
   genuinely an invisible state — both sides empty:true)                                       
 - 25 unit + 6 integration tests; fmt + clippy -D warnings clean                               
                                                                                               
 Deployment                                                                                    
                                                                                               
 - Dockerfile: provided:al2023 build with RUSTFLAGS="-C target-cpu=x86-64-v2", ships only the  
   static bootstrap (no cwebp, no resvg CLI). Built amd64 locally — an in-container run        
   produced a byte-identical PNG (sha256 match) vs the host harness.                           
 - CDK: componentRasterBackend: 'python' | 'rust' mirrors your compose rollout.                
   aqw-char-dev-componentraster-rust is deployed as an isolated candidate at reserved          
   concurrency 1 (work-bucket-only IAM, own log group + alarm) until you flip the switch; the  
   Distributed Map then points RasterComponentState at it. 11 Lambdas total; npm run           
   build/test/synth pass.                                                                      
                                                                                               
 Your move (needs AWS)                                                                         
                                                                                               
 1. aws sso login --profile aqw-char-dev → npm run diff (expect only the new                   
    function/role/policy/log-group/alarm) → npm run deploy                                     
 2. Direct-invoke comparisons on a saved job's task set (Python vs Rust), then set             
    componentRasterBackend: 'rust' and run the one-concurrency end-to-end sequence (same steps 
    as the compose rollout).                                                                   
                                                                                               
 One heads-up worth knowing: if you ever want byte-identical output and FIR, the honest path   
 is the Pillow-exact resampler (as implemented); FIR is the fast approximate option.           
