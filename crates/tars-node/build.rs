// napi-build emits the symbol-export shim N-API needs to be a valid
// Node native addon (the `napi_register_module_v1` glue). Without
// this, `import('./tars_node.node')` would crash on load with
// "module did not self-register".
extern crate napi_build;

fn main() {
    napi_build::setup();
}
