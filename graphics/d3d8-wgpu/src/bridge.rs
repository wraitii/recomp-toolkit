#[cxx::bridge(namespace = "recomp::d3d")]
pub mod ffi {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum StatusCode {
        Ok = 0,
        InvalidArgument = 1,
        Unsupported = 2,
        Backend = 3,
        NotImplemented = 4,
        OutOfMemory = 5,
        WasStillDrawing = 6,
        DeviceLost = 7,
        DeviceNotReset = 8,
        NoAutogen = 9,
        NotAvailable = 10,
        InvalidData = 11,
    }

    struct Status {
        code: StatusCode,
        message: String,
        state_block_error: StateBlockError,
    }

    struct ShaderHandleResult {
        status: Status,
        handle: u32,
    }
    struct VertexStream<'a> {
        stream: u32,
        stride: u32,
        data: &'a [u8],
    }

    struct ConstantRead {
        status: Status,
        values: Vec<u32>,
    }

    struct ValueResult {
        status: Status,
        value: u32,
    }

    struct ValuesResult {
        status: Status,
        values: Vec<u32>,
    }

    struct FloatValuesResult {
        status: Status,
        values: Vec<f32>,
    }

    struct HandleResult {
        status: Status,
        handle: u32,
    }

    struct TextureSetResult {
        status: Status,
        old_id: u32,
        new_id: u32,
        recording: bool,
    }

    struct TextureBindings {
        old_values: Vec<u32>,
        new_values: Vec<u32>,
    }

    struct ApplyStateBlockResult {
        status: Status,
        bindings: TextureBindings,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum StateBlockError {
        None = 0,
        InBeginStateBlock = 1,
        NotInBeginStateBlock = 2,
        InvalidStateBlock = 3,
        OutOfMemory = 4,
    }

    struct AdapterInfo {
        name: String,
        vendor_id: u32,
        device_id: u32,
        max_texture_dimension_2d: u32,
        max_bind_groups: u32,
        max_vertex_buffers: u32,
        max_vertex_attributes: u32,
    }
    struct AdapterInfoResult {
        status: Status,
        info: AdapterInfo,
    }
    struct Rect {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
    }
    struct TextureLevel<'a> {
        level: u32,
        generation: u64,
        force_upload: bool,
        width: u32,
        height: u32,
        data: &'a [u8],
    }
    #[derive(Clone, Copy)]
    struct Viewport {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        min_z: f32,
        max_z: f32,
    }
    struct ViewportResult {
        status: Status,
        viewport: Viewport,
    }
    struct Material {
        diffuse_r: f32,
        diffuse_g: f32,
        diffuse_b: f32,
        diffuse_a: f32,
        ambient_r: f32,
        ambient_g: f32,
        ambient_b: f32,
        ambient_a: f32,
        specular_r: f32,
        specular_g: f32,
        specular_b: f32,
        specular_a: f32,
        emissive_r: f32,
        emissive_g: f32,
        emissive_b: f32,
        emissive_a: f32,
        power: f32,
    }
    struct MaterialResult {
        status: Status,
        material: Material,
    }
    struct Light {
        light_type: u32,
        diffuse_r: f32,
        diffuse_g: f32,
        diffuse_b: f32,
        diffuse_a: f32,
        specular_r: f32,
        specular_g: f32,
        specular_b: f32,
        specular_a: f32,
        ambient_r: f32,
        ambient_g: f32,
        ambient_b: f32,
        ambient_a: f32,
        position_x: f32,
        position_y: f32,
        position_z: f32,
        direction_x: f32,
        direction_y: f32,
        direction_z: f32,
        range: f32,
        falloff: f32,
        attenuation0: f32,
        attenuation1: f32,
        attenuation2: f32,
        theta: f32,
        phi: f32,
    }
    struct LightResult {
        status: Status,
        light: Light,
    }
    struct FrameHandoff {
        texture: *mut u8,
        lease: usize,
        width: u32,
        height: u32,
    }
    struct FrameHandoffResult {
        status: Status,
        handoff: FrameHandoff,
    }

    #[derive(Clone, Copy)]
    struct D3d9PresentParams {
        width: u32,
        height: u32,
        format: u32,
        count: u32,
        multisample_type: u32,
        multisample_quality: u32,
        swap_effect: u32,
        windowed: u32,
        enable_auto_depth_stencil: u32,
        auto_depth_stencil_format: u32,
        flags: u32,
        fullscreen_refresh_rate: u32,
        presentation_interval: u32,
    }

    #[derive(Clone, Copy)]
    struct D3d9DisplayMode {
        width: u32,
        height: u32,
        refresh_rate: u32,
        format: u32,
    }

    struct D3d9DeviceRequest {
        adapter: u32,
        device_type: u32,
        behavior_flags: u32,
        present: D3d9PresentParams,
        display_mode: D3d9DisplayMode,
    }

    struct D3d9CreateResult {
        status: Status,
        present: D3d9PresentParams,
    }

    #[derive(Clone, Copy)]
    struct D3d9CreationParameters {
        adapter: u32,
        device_type: u32,
        behavior_flags: u32,
    }

    struct D3d9CreationParametersResult {
        status: Status,
        parameters: D3d9CreationParameters,
    }

    struct D3d9ResetRequest {
        present: D3d9PresentParams,
        display_mode: D3d9DisplayMode,
        implicit_external_refs: bool,
        window_valid: bool,
    }

    struct D3d9ResetResult {
        status: Status,
        present: D3d9PresentParams,
    }

    struct D3d9AdapterIdentifierResult {
        status: Status,
        description: String,
        vendor_id: u32,
        device_id: u32,
        sub_sys_id: u32,
        revision: u32,
        whql_level: u32,
        driver_version_low: u32,
        driver_version_high: u32,
    }

    struct D3d9CapsResult {
        status: Status,
        caps: Vec<u8>,
    }

    struct D3d9FrameResult {
        status: Status,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    }

    struct D3d9DisplayModeResult {
        status: Status,
        width: u32,
        height: u32,
        refresh_rate: u32,
        format: u32,
    }

    struct D3d9DisplayModeRequest {
        swap_chain: u32,
        width: u32,
        height: u32,
        refresh_rate: u32,
        format: u32,
    }

    struct D3d9CheckDeviceTypeRequest {
        adapter: u32,
        device_type: u32,
        adapter_format: u32,
        backbuffer_format: u32,
        windowed: u32,
    }

    struct D3d9CheckDeviceFormatRequest {
        adapter: u32,
        device_type: u32,
        adapter_format: u32,
        usage: u32,
        resource_type: u32,
        check_format: u32,
        windowed: u32,
    }

    struct D3d9CheckMultiSampleRequest {
        adapter: u32,
        device_type: u32,
        surface_format: u32,
        windowed: u32,
        multisample_type: u32,
    }

    struct D3d9CheckDepthStencilRequest {
        adapter: u32,
        device_type: u32,
        adapter_format: u32,
        render_target_format: u32,
        depth_stencil_format: u32,
    }

    struct D3d9CheckFormatConversionRequest {
        adapter: u32,
        device_type: u32,
        source_format: u32,
        destination_format: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum D3d9BufferKind {
        Vertex,
        Index,
    }

    struct D3d9BufferRequest {
        kind: D3d9BufferKind,
        size: u32,
        usage: u32,
        pool: u32,
        fvf: u32,
        format: u32,
        shared_handle: bool,
    }

    struct D3d9GuestBufferCreateResult {
        status: Status,
        request: D3d9BufferRequest,
    }

    struct D3d9BufferUnlockResult {
        status: Status,
        closed: bool,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum D3d9TextureKind {
        Texture2D,
        Cube,
        Volume,
    }

    struct D3d9TextureRequest {
        kind: D3d9TextureKind,
        width: u32,
        height: u32,
        depth: u32,
        levels: u32,
        usage: u32,
        format: u32,
        pool: u32,
        shared_handle: bool,
    }

    struct D3d9SurfaceRequest {
        width: u32,
        height: u32,
        format: u32,
        pool: u32,
        usage: u32,
        multisample_type: u32,
        multisample_quality: u32,
        shared_handle: bool,
        lockable: bool,
        discard: bool,
    }

    struct D3d9GuestTextureCreateResult {
        status: Status,
        request: D3d9TextureRequest,
    }
    struct D3d9GuestSurfaceCreateResult {
        status: Status,
        request: D3d9SurfaceRequest,
    }
    struct D3d9GuestRectRequest {
        level: u32,
        flags: u32,
        full_region: bool,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    struct D3d9GuestRectRequestResult {
        status: Status,
        request: D3d9GuestRectRequest,
    }

    struct D3d9TextureCreateResult {
        status: Status,
        no_autogen: bool,
    }

    #[derive(Clone, Copy)]
    struct D3dxImageInfo {
        width: u32,
        height: u32,
        depth: u32,
        mip_levels: u32,
        format: u32,
        resource_type: u32,
        image_file_format: u32,
    }

    struct D3dxImageInfoResult {
        status: Status,
        info: D3dxImageInfo,
    }

    struct D3dxCreateRequest {
        kind: D3d9TextureKind,
        width: u32,
        height: u32,
        depth: u32,
        levels: u32,
        usage: u32,
        format: u32,
        pool: u32,
        filter: u32,
        mip_filter: u32,
        color_key: u32,
    }

    struct D3dxCreateResult {
        status: Status,
        info: D3dxImageInfo,
    }

    struct D3dxSurfaceReadResult {
        status: Status,
        snapshot: Box<D3dxSurfaceData>,
    }

    struct D3dxBytesResult {
        status: Status,
        bytes: Vec<u8>,
    }

    #[derive(Clone, Copy)]
    struct D3dxConstantTableDesc {
        creator_offset: u32,
        version: u32,
        constants: u32,
    }

    struct D3dxConstantTableResult {
        status: Status,
        buffer: Vec<u8>,
        desc: D3dxConstantTableDesc,
        node_count: u32,
        invalid_bytecode: bool,
    }

    #[derive(Clone)]
    struct D3dxConstantDesc {
        name_offset: u32,
        register_set: u32,
        register_index: u32,
        register_count: u32,
        class_: u32,
        type_: u32,
        rows: u32,
        columns: u32,
        elements: u32,
        struct_members: u32,
        bytes: u32,
        default_offset: u32,
    }

    struct D3dxConstantDescResult {
        status: Status,
        desc: D3dxConstantDesc,
    }

    #[derive(Clone, Copy)]
    struct D3dxEffectDesc {
        parameters: u32,
        techniques: u32,
        functions: u32,
    }

    struct D3dxEffectDescResult {
        status: Status,
        desc: D3dxEffectDesc,
    }

    struct D3dxEffectCreateResult {
        status: Status,
        parameters: u32,
        techniques: u32,
    }

    #[derive(Clone, Copy, Default)]
    struct D3dxEffectParamDesc {
        class_: u32,
        type_: u32,
        rows: u32,
        columns: u32,
        elements: u32,
        annotations: u32,
        struct_members: u32,
        flags: u32,
        bytes: u32,
    }

    struct D3dxEffectParamDescResult {
        status: Status,
        desc: D3dxEffectParamDesc,
        name: Vec<u8>,
        semantic: Vec<u8>,
    }

    #[derive(Clone, Copy, Default)]
    struct D3dxEffectTechniqueDesc {
        passes: u32,
        annotations: u32,
    }

    struct D3dxEffectTechniqueDescResult {
        status: Status,
        desc: D3dxEffectTechniqueDesc,
        name: Vec<u8>,
    }

    #[derive(Default)]
    struct D3dxEffectPassDesc {
        annotations: u32,
        vertex_shader: Vec<u8>,
        pixel_shader: Vec<u8>,
    }

    struct D3dxEffectPassDescResult {
        status: Status,
        desc: D3dxEffectPassDesc,
        name: Vec<u8>,
    }

    struct D3dxEffectShadersResult {
        status: Status,
        vertex: Vec<u32>,
        pixel: Vec<u32>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum D3dxEffectCommandKind {
        SetRenderState = 0,
        SetTextureStageState = 1,
        SetSamplerState = 2,
        SetTransform = 3,
        SetTexture = 4,
        SetVertexShader = 5,
        SetPixelShader = 6,
        SetVertexShaderConstantF = 7,
        SetPixelShaderConstantF = 8,
        SetFVF = 9,
    }

    struct D3dxEffectCommand {
        kind: D3dxEffectCommandKind,
        index: u32,
        state: u32,
        value: u32,
        offset: u32,
        count: u32,
    }

    struct D3dxEffectRunResult {
        status: Status,
        commands: Vec<D3dxEffectCommand>,
        values: Vec<f32>,
        reserved: bool,
        bindings_changed: bool,
    }

    struct D3dxTextureRefCounts {
        first: u32,
        second: u32,
    }

    #[derive(Clone, Copy)]
    struct D3dxSemantic {
        usage: u32,
        usage_index: u32,
    }

    struct D3dxSemanticsResult {
        status: Status,
        semantics: Vec<D3dxSemantic>,
    }

    struct D3dxProfilesResult {
        status: Status,
        vertex: String,
        pixel: String,
    }

    struct D3dxMatrixResult {
        status: Status,
        values: [f32; 16],
    }

    struct D3dxMatrixInverseResult {
        status: Status,
        valid: bool,
        determinant: f32,
        values: [f32; 16],
    }

    #[derive(Clone, Copy)]
    struct D3d9TextureDescriptor {
        format: u32,
        resource_type: u32,
        usage: u32,
        pool: u32,
        width: u32,
        height: u32,
        depth: u32,
        levels: u32,
    }

    #[derive(Clone, Copy)]
    struct D3d9SurfaceDescriptor {
        format: u32,
        resource_type: u32,
        usage: u32,
        pool: u32,
        multisample_type: u32,
        multisample_quality: u32,
        width: u32,
        height: u32,
    }

    #[derive(Clone, Copy)]
    struct D3d9LevelDescriptor {
        format: u32,
        width: u32,
        height: u32,
        depth: u32,
        levels: u32,
    }

    struct D3d9TextureDescriptorResult {
        status: Status,
        descriptor: D3d9TextureDescriptor,
    }

    struct D3d9SurfaceDescriptorResult {
        status: Status,
        descriptor: D3d9SurfaceDescriptor,
    }

    struct D3d9LevelDescriptorResult {
        status: Status,
        descriptor: D3d9LevelDescriptor,
    }

    #[derive(Clone, Copy)]
    struct D3d9LockRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct D3d9Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[derive(Clone, Copy)]
    struct D3d9LockBox {
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
        front: u32,
        back: u32,
    }

    struct D3d9LevelLockRequest {
        face: u32,
        level: u32,
        volume: bool,
        full_region: bool,
        rect: D3d9LockRect,
        volume_box: D3d9LockBox,
        flags: u32,
    }

    struct D3d9LevelLockResult {
        status: Status,
        pitch: u32,
        slice_pitch: u32,
        offset: u32,
        size: u32,
        total: u32,
    }

    struct D3d9PrivateDataResult {
        status: Status,
        found: bool,
        data: Vec<u8>,
    }

    struct D3d9VertexElement {
        stream: u16,
        offset: u16,
        element_type: u8,
        method: u8,
        usage: u8,
        usage_index: u8,
    }

    struct D3d9Bindings {
        textures: [u32; 16],
        streams: [u32; 16],
        indices: u32,
        vertex_shader: u32,
        pixel_shader: u32,
        declaration: u32,
    }

    struct D3d9BindingsResult {
        status: Status,
        live: D3d9Bindings,
        recording: D3d9Bindings,
        recording_active: bool,
    }

    struct D3d9BindingSnapshot {
        live: D3d9Bindings,
        recording: D3d9Bindings,
        recording_active: bool,
        render_target: u32,
        depth_stencil: u32,
    }

    struct D3d9BlockBindingsResult {
        status: Status,
        bindings: D3d9Bindings,
    }

    struct D3d9ScissorResult {
        status: Status,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    struct D3d9StreamResult {
        status: Status,
        buffer: u32,
        offset: u32,
        stride: u32,
    }

    struct D3d9LightResult {
        status: Status,
        light_type: u32,
        values: Vec<f32>,
    }

    struct D3d9DeclarationResult {
        status: Status,
        elements: Vec<D3d9VertexElement>,
    }

    extern "Rust" {
        type Device;
        type Storage;
        type Device7State;
        type Resource;
        type D3d9Device;
        type D3d9Resource;
        type GuestBuffer;
        type GuestBufferBudget;
        type GuestTexture;
        type D3dxSurfaceData;

        fn new_device() -> Box<Device>;
        fn new_d3d9_device() -> Box<D3d9Device>;
        fn d3d9_available_texture_mem(device: &D3d9Device) -> ValueResult;
        fn d3d9_create_device(
            device: &mut D3d9Device,
            request: &D3d9DeviceRequest,
        ) -> D3d9CreateResult;
        fn d3d9_creation_parameters(device: &D3d9Device) -> D3d9CreationParametersResult;
        fn d3d9_set_render_state(device: &mut D3d9Device, state: u32, value: u32) -> Status;
        fn d3d9_get_render_state(device: &D3d9Device, state: u32) -> ValueResult;
        fn d3d9_set_texture_stage_state(
            device: &mut D3d9Device,
            stage: u32,
            state: u32,
            value: u32,
        ) -> Status;
        fn d3d9_get_texture_stage_state(device: &D3d9Device, stage: u32, state: u32)
        -> ValueResult;
        fn d3d9_set_sampler_state(
            device: &mut D3d9Device,
            sampler: u32,
            state: u32,
            value: u32,
        ) -> Status;
        fn d3d9_get_sampler_state(device: &D3d9Device, sampler: u32, state: u32) -> ValueResult;
        fn d3d9_set_transform(device: &mut D3d9Device, state: u32, values: &[f32]) -> Status;
        fn d3d9_get_transform(device: &D3d9Device, state: u32) -> FloatValuesResult;
        fn d3d9_multiply_transform(device: &mut D3d9Device, state: u32, values: &[f32]) -> Status;
        fn d3d9_set_viewport(device: &mut D3d9Device, viewport: &Viewport) -> Status;
        fn d3d9_get_viewport(device: &D3d9Device) -> ViewportResult;
        fn d3d9_set_scissor(
            device: &mut D3d9Device,
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        ) -> Status;
        fn d3d9_get_scissor(device: &D3d9Device) -> D3d9ScissorResult;
        fn d3d9_set_material(device: &mut D3d9Device, values: &[f32]) -> Status;
        fn d3d9_get_material(device: &D3d9Device) -> FloatValuesResult;
        fn d3d9_set_light(
            device: &mut D3d9Device,
            index: u32,
            light_type: u32,
            values: &[f32],
        ) -> Status;
        fn d3d9_get_light(device: &D3d9Device, index: u32) -> D3d9LightResult;
        fn d3d9_set_light_enable(device: &mut D3d9Device, index: u32, enable: bool) -> Status;
        fn d3d9_get_light_enable(device: &D3d9Device, index: u32) -> ValueResult;
        fn d3d9_set_clip_plane(device: &mut D3d9Device, index: u32, values: &[f32]) -> Status;
        fn d3d9_get_clip_plane(device: &D3d9Device, index: u32) -> FloatValuesResult;
        fn d3d9_set_texture(device: &mut D3d9Device, stage: u32, id: u32) -> Status;
        fn d3d9_get_texture(device: &D3d9Device, stage: u32) -> ValueResult;
        fn d3d9_set_stream_source(
            device: &mut D3d9Device,
            stream: u32,
            buffer: u32,
            offset: u32,
            stride: u32,
        ) -> Status;
        fn d3d9_get_stream_source(device: &D3d9Device, stream: u32) -> D3d9StreamResult;
        fn d3d9_set_stream_freq(device: &mut D3d9Device, stream: u32, divider: u32) -> Status;
        fn d3d9_get_stream_freq(device: &D3d9Device, stream: u32) -> ValueResult;
        fn d3d9_set_indices(device: &mut D3d9Device, id: u32) -> Status;
        fn d3d9_get_indices(device: &D3d9Device) -> ValueResult;
        fn d3d9_create_fvf_declaration(device: &mut D3d9Device, fvf: u32) -> ValueResult;
        fn d3d9_set_fvf_binding(device: &mut D3d9Device, handle: u32, fvf: u32) -> Status;
        fn d3d9_get_fvf(device: &D3d9Device) -> ValueResult;
        fn d3d9_create_vertex_declaration(
            device: &mut D3d9Device,
            elements: &[D3d9VertexElement],
        ) -> ValueResult;
        fn d3d9_destroy_declaration(device: &mut D3d9Device, handle: u32) -> Status;
        fn d3d9_declaration_elements(device: &D3d9Device, handle: u32) -> D3d9DeclarationResult;
        fn d3d9_set_vertex_declaration(device: &mut D3d9Device, handle: u32) -> Status;
        fn d3d9_get_vertex_declaration(device: &D3d9Device) -> ValueResult;
        fn d3d9_create_shader(device: &mut D3d9Device, pixel: bool, code: &[u32]) -> ValueResult;
        fn d3d9_destroy_shader(device: &mut D3d9Device, pixel: bool, handle: u32) -> Status;
        fn d3d9_set_shader(device: &mut D3d9Device, pixel: bool, handle: u32) -> Status;
        fn d3d9_get_shader(device: &D3d9Device, pixel: bool) -> ValueResult;
        fn d3d9_shader_function(device: &D3d9Device, pixel: bool, handle: u32) -> ValuesResult;
        fn d3d9_shader_size(device: &D3d9Device, pixel: bool, handle: u32) -> ValueResult;
        fn d3d9_set_shader_constants_f(
            device: &mut D3d9Device,
            pixel: bool,
            start: u32,
            values: &[f32],
        ) -> Status;
        fn d3d9_get_shader_constants_f(
            device: &D3d9Device,
            pixel: bool,
            start: u32,
            count: u32,
        ) -> FloatValuesResult;
        fn d3d9_set_shader_constants_i(
            device: &mut D3d9Device,
            pixel: bool,
            start: u32,
            values: &[i32],
        ) -> Status;
        fn d3d9_get_shader_constants_i(
            device: &D3d9Device,
            pixel: bool,
            start: u32,
            count: u32,
        ) -> ValuesResult;
        fn d3d9_set_shader_constants_b(
            device: &mut D3d9Device,
            pixel: bool,
            start: u32,
            values: &[u32],
        ) -> Status;
        fn d3d9_get_shader_constants_b(
            device: &D3d9Device,
            pixel: bool,
            start: u32,
            count: u32,
        ) -> ValuesResult;
        fn d3d9_create_state_block(device: &mut D3d9Device, block_type: u32) -> ValueResult;
        fn d3d9_begin_state_block(device: &mut D3d9Device) -> Status;
        fn d3d9_end_state_block(device: &mut D3d9Device) -> ValueResult;
        fn d3d9_capture_state_block(device: &mut D3d9Device, handle: u32) -> Status;
        fn d3d9_apply_state_block(device: &mut D3d9Device, handle: u32) -> Status;
        fn d3d9_destroy_state_block(device: &mut D3d9Device, handle: u32) -> Status;
        fn d3d9_state_block_bindings(device: &D3d9Device, handle: u32) -> D3d9BlockBindingsResult;
        fn d3d9_current_bindings(device: &D3d9Device) -> D3d9BindingSnapshot;
        fn d3d9_note_binding_refresh(unchanged: bool, effect_pass: bool);
        fn guest_status_hresult(status: &Status) -> u32;
        fn new_guest_texture() -> Box<GuestTexture>;
        unsafe fn guest_texture_prepare_create(
            callbacks: usize,
            out: u32,
            width: u32,
            height: u32,
            levels: u32,
            usage: u32,
            format: u32,
            pool: u32,
            shared_handle: bool,
        ) -> D3d9GuestTextureCreateResult;
        unsafe fn guest_surface_prepare_create(
            callbacks: usize,
            out: u32,
            width: u32,
            height: u32,
            format: u32,
            offscreen: bool,
            pool_or_multisample: u32,
            quality: u32,
            lockable_or_discard: bool,
            usage: u32,
            shared_handle: bool,
        ) -> D3d9GuestSurfaceCreateResult;
        unsafe fn guest_rect_prepare(
            callbacks: usize,
            out: u32,
            rect: u32,
            level: u32,
            flags: u32,
        ) -> D3d9GuestRectRequestResult;
        unsafe fn guest_rect_lock(
            state: &mut GuestTexture,
            resource: &mut D3d9Resource,
            callbacks: usize,
            request: &D3d9GuestRectRequest,
            out: u32,
        ) -> Status;
        unsafe fn guest_rect_prepare_unlock(
            state: &GuestTexture,
            callbacks: usize,
            level: u32,
        ) -> Status;
        unsafe fn guest_rect_unlock(
            state: &mut GuestTexture,
            resource: &mut D3d9Resource,
            callbacks: usize,
            level: u32,
        ) -> Status;
        unsafe fn guest_texture_release(state: &mut GuestTexture, callbacks: usize) -> Status;
        unsafe fn guest_surface_desc(
            resource: &D3d9Resource,
            callbacks: usize,
            out: u32,
            level: u32,
            texture_view: bool,
        ) -> Status;
        fn new_guest_buffer() -> Box<GuestBuffer>;
        fn new_guest_buffer_budget() -> Box<GuestBufferBudget>;
        unsafe fn guest_buffer_prepare_create(
            callbacks: usize,
            out: u32,
            vertex: bool,
            length: u32,
            usage: u32,
            format_or_fvf: u32,
            pool: u32,
            shared_handle: bool,
        ) -> D3d9GuestBufferCreateResult;
        unsafe fn guest_publish_interface(callbacks: usize, out: u32, view: u32) -> Status;
        unsafe fn guest_buffer_lock(
            buffer: &mut GuestBuffer,
            budget: &mut GuestBufferBudget,
            resource: &mut D3d9Resource,
            callbacks: usize,
            offset: u32,
            size: u32,
            out: u32,
            flags: u32,
        ) -> Status;
        unsafe fn guest_buffer_unlock(
            buffer: &mut GuestBuffer,
            budget: &mut GuestBufferBudget,
            resource: &mut D3d9Resource,
            callbacks: usize,
        ) -> Status;
        unsafe fn guest_buffer_sync(
            buffer: &mut GuestBuffer,
            resource: &mut D3d9Resource,
            callbacks: usize,
        ) -> Status;
        unsafe fn guest_buffer_release(
            buffer: &mut GuestBuffer,
            budget: &mut GuestBufferBudget,
            callbacks: usize,
        ) -> Status;
        unsafe fn guest_buffer_descriptor(
            resource: &D3d9Resource,
            callbacks: usize,
            out: u32,
            vertex: bool,
        ) -> Status;
        fn new_d3d9_resource() -> Box<D3d9Resource>;
        fn d3d9_create_buffer(
            device: &D3d9Device,
            resource: &mut D3d9Resource,
            request: &D3d9BufferRequest,
        ) -> Status;
        fn d3d9_create_texture(
            device: &D3d9Device,
            resource: &mut D3d9Resource,
            request: &D3d9TextureRequest,
        ) -> D3d9TextureCreateResult;
        fn d3d9_create_surface(
            device: &D3d9Device,
            resource: &mut D3d9Resource,
            request: &D3d9SurfaceRequest,
        ) -> Status;
        fn d3d9_d3dx_image_info(bytes: &[u8]) -> D3dxImageInfoResult;
        fn d3d9_d3dx_create_texture(
            device: &D3d9Device,
            resource: &mut D3d9Resource,
            request: &D3dxCreateRequest,
            bytes: &[u8],
        ) -> D3dxCreateResult;
        fn d3d9_d3dx_surface_read(
            device: &mut D3d9Device,
            resource: &mut D3d9Resource,
            face: u32,
            level: u32,
            rect: &D3d9Rect,
            rect_present: bool,
            color_key: u32,
        ) -> D3dxSurfaceReadResult;
        fn d3d9_d3dx_surface_store(
            device: &mut D3d9Device,
            resource: &mut D3d9Resource,
            face: u32,
            level: u32,
            rect: &D3d9Rect,
            rect_present: bool,
            snapshot: &D3dxSurfaceData,
            filter: u32,
        ) -> Status;
        fn d3d9_d3dx_save_surface(
            device: &mut D3d9Device,
            resource: &mut D3d9Resource,
            face: u32,
            level: u32,
            rect: &D3d9Rect,
            rect_present: bool,
            format: u32,
        ) -> D3dxBytesResult;
        fn d3d9_d3dx_save_texture(
            device: &mut D3d9Device,
            resource: &mut D3d9Resource,
            format: u32,
        ) -> D3dxBytesResult;
        type D3dxConstantTable;
        fn new_d3dx_constant_table() -> Box<D3dxConstantTable>;
        fn d3dx_constant_table_info(table: &D3dxConstantTable) -> D3dxConstantTableDesc;
        fn d3dx_constant_table_create(
            table: &mut D3dxConstantTable,
            code: &[u32],
        ) -> D3dxConstantTableResult;
        fn d3dx_constant_table_desc(table: &D3dxConstantTable, node: u32)
        -> D3dxConstantDescResult;
        fn d3dx_constant_table_find(table: &D3dxConstantTable, parent: u32, name: &[u8]) -> i32;
        fn d3dx_constant_table_constant(table: &D3dxConstantTable, parent: u32, index: u32) -> i32;
        fn d3dx_constant_table_element(table: &D3dxConstantTable, node: u32, index: u32) -> i32;
        fn d3dx_constant_table_sampler(table: &D3dxConstantTable, node: u32) -> u32;
        fn d3dx_constant_table_apply(
            device: &mut D3d9Device,
            table: &D3dxConstantTable,
            node: u32,
            setter: u32,
            data: &[u8],
        ) -> Status;
        fn d3dx_constant_table_defaults(
            device: &mut D3d9Device,
            table: &D3dxConstantTable,
        ) -> Status;
        fn d3dx_constant_table_input_size(
            table: &D3dxConstantTable,
            node: u32,
            setter: u32,
            requested_bytes: u64,
        ) -> u64;
        fn d3dx_shader_version(code: &[u32]) -> u32;
        fn d3dx_shader_input_semantics(code: &[u32]) -> D3dxSemanticsResult;
        fn d3dx_shader_profiles(device: &D3d9Device) -> D3dxProfilesResult;
        fn d3dx_matrix_multiply(a: &[f32], b: &[f32]) -> D3dxMatrixResult;
        fn d3dx_matrix_transpose(a: &[f32]) -> D3dxMatrixResult;
        fn d3dx_matrix_inverse(a: &[f32]) -> D3dxMatrixInverseResult;
        type D3dxEffect;
        type D3dxEffectPool;
        fn new_d3dx_effect() -> Box<D3dxEffect>;
        fn new_d3dx_effect_pool() -> Box<D3dxEffectPool>;
        fn d3dx_effect_pool_id(pool: &D3dxEffectPool) -> u32;
        fn d3dx_effect_create(
            effect: &mut D3dxEffect,
            device: &mut D3d9Device,
            bytes: &[u8],
            flags: u32,
        ) -> D3dxEffectCreateResult;
        fn d3dx_effect_join_pool(
            effect: &mut D3dxEffect,
            pool: &D3dxEffectPool,
            device_id: u32,
        ) -> Status;
        fn d3dx_effect_desc(effect: &D3dxEffect) -> D3dxEffectDescResult;
        fn d3dx_effect_param_desc(effect: &D3dxEffect, handle: u32) -> D3dxEffectParamDescResult;
        fn d3dx_effect_param_name(effect: &D3dxEffect, handle: u32) -> D3dxBytesResult;
        fn d3dx_effect_param_semantic(effect: &D3dxEffect, handle: u32) -> D3dxBytesResult;
        fn d3dx_effect_technique_desc(
            effect: &D3dxEffect,
            handle: u32,
        ) -> D3dxEffectTechniqueDescResult;
        fn d3dx_effect_pass_desc(effect: &D3dxEffect, handle: u32) -> D3dxEffectPassDescResult;
        fn d3dx_effect_pass_shader(
            effect: &D3dxEffect,
            handle: u32,
            pixel: bool,
        ) -> D3dxBytesResult;
        fn d3dx_effect_find(effect: &D3dxEffect, parent: u32, name: &[u8], kind: u32) -> u32;
        fn d3dx_effect_enum(effect: &D3dxEffect, parent: u32, index: u32, kind: u32) -> u32;
        fn d3dx_effect_set(
            effect: &mut D3dxEffect,
            handle: u32,
            setter: u32,
            data: &[u8],
        ) -> Status;
        fn d3dx_effect_set_raw(
            effect: &mut D3dxEffect,
            handle: u32,
            offset: u32,
            data: &[u8],
        ) -> Status;
        fn d3dx_effect_get(effect: &D3dxEffect, handle: u32, kind: u32) -> D3dxBytesResult;
        fn d3dx_effect_set_texture(effect: &mut D3dxEffect, handle: u32, id: u32) -> ValueResult;
        fn d3dx_effect_texture(effect: &D3dxEffect, handle: u32) -> ValueResult;
        fn d3dx_effect_texture_shared(effect: &D3dxEffect, handle: u32) -> bool;
        fn d3dx_effect_texture_ref_counts(
            effect: &D3dxEffect,
            first: u32,
            second: u32,
        ) -> D3dxTextureRefCounts;
        fn d3dx_effect_pool_texture_ref_counts(
            pool: &D3dxEffectPool,
            first: u32,
            second: u32,
        ) -> D3dxTextureRefCounts;
        fn d3dx_effect_set_technique(effect: &mut D3dxEffect, handle: u32) -> Status;
        fn d3dx_effect_current_technique(effect: &D3dxEffect) -> u32;
        fn d3dx_effect_validate_technique(effect: &D3dxEffect, handle: u32) -> Status;
        fn d3dx_effect_find_next_valid(effect: &D3dxEffect, handle: u32) -> u32;
        fn d3dx_effect_is_parameter_used(effect: &D3dxEffect, param: u32, tech: u32) -> bool;
        fn d3dx_effect_flags(effect: &D3dxEffect, handle: u32) -> u32;
        fn d3dx_effect_pool_id_of(effect: &D3dxEffect) -> u32;
        fn d3dx_effect_prepare_shaders(
            effect: &mut D3dxEffect,
            device: &mut D3d9Device,
        ) -> D3dxEffectShadersResult;
        fn d3dx_effect_begin(
            effect: &mut D3dxEffect,
            device: &mut D3d9Device,
            flags: u32,
        ) -> ValueResult;
        fn d3dx_effect_begin_pass(
            effect: &mut D3dxEffect,
            device: &mut D3d9Device,
            index: u32,
        ) -> D3dxEffectRunResult;
        fn d3dx_effect_commit(
            effect: &mut D3dxEffect,
            device: &mut D3d9Device,
        ) -> D3dxEffectRunResult;
        fn d3dx_effect_batch_finish(effect: &mut D3dxEffect, ok: bool) -> Status;
        fn d3dx_effect_recycle(
            effect: &mut D3dxEffect,
            commands: Vec<D3dxEffectCommand>,
            values: Vec<f32>,
        );
        fn d3dx_effect_batch_active(effect: &D3dxEffect) -> bool;
        fn d3dx_effect_set_state_manager(effect: &mut D3dxEffect, active: bool) -> Status;
        fn d3dx_effect_end_pass(effect: &mut D3dxEffect) -> Status;
        fn d3dx_effect_end(effect: &mut D3dxEffect, device: &mut D3d9Device) -> Status;
        fn d3dx_effect_on_lost(effect: &mut D3dxEffect, device: &mut D3d9Device) -> Status;
        fn d3dx_effect_on_reset(effect: &mut D3dxEffect, device: &mut D3d9Device) -> Status;
        fn d3dx_effect_release(effect: &mut D3dxEffect);
        fn d3dx_effect_texture_refs(effect: &D3dxEffect) -> Vec<u32>;
        fn d3dx_effect_pool_texture_refs(pool: &D3dxEffectPool) -> Vec<u32>;
        fn d3dx_effect_saved_bindings(effect: &D3dxEffect) -> D3d9BindingsResult;
        fn d3dx_effect_shutdown(effect: &mut D3dxEffect, device: &mut D3d9Device);
        fn d3d9_resource_type(resource: &D3d9Resource) -> ValueResult;
        fn d3d9_texture_descriptor(resource: &D3d9Resource) -> D3d9TextureDescriptorResult;
        fn d3d9_surface_descriptor(resource: &D3d9Resource) -> D3d9SurfaceDescriptorResult;
        fn d3d9_texture_level_descriptor(
            resource: &D3d9Resource,
            face: u32,
            level: u32,
        ) -> D3d9LevelDescriptorResult;
        fn d3d9_texture_volume_descriptor(
            resource: &D3d9Resource,
            level: u32,
        ) -> D3d9LevelDescriptorResult;
        fn d3d9_level_lock_prepare(
            resource: &mut D3d9Resource,
            request: &D3d9LevelLockRequest,
        ) -> D3d9LevelLockResult;
        fn d3d9_level_lock_commit(
            resource: &mut D3d9Resource,
            request: &D3d9LevelLockRequest,
        ) -> D3d9LevelLockResult;
        fn d3d9_level_bytes(resource: &D3d9Resource, face: u32, level: u32) -> &[u8];
        fn d3d9_level_unlock(
            resource: &mut D3d9Resource,
            face: u32,
            level: u32,
            bytes: &[u8],
        ) -> D3d9BufferUnlockResult;
        fn d3d9_set_priority(resource: &mut D3d9Resource, priority: u32) -> ValueResult;
        fn d3d9_priority(resource: &D3d9Resource) -> ValueResult;
        fn d3d9_set_lod(resource: &mut D3d9Resource, lod: u32) -> ValueResult;
        fn d3d9_lod(resource: &D3d9Resource) -> ValueResult;
        fn d3d9_set_private_data(
            resource: &mut D3d9Resource,
            view: u32,
            guid: &[u8],
            data: &[u8],
            flags: u32,
        ) -> Status;
        fn d3d9_get_private_data(
            resource: &D3d9Resource,
            view: u32,
            guid: &[u8],
        ) -> D3d9PrivateDataResult;
        fn d3d9_free_private_data(resource: &mut D3d9Resource, view: u32, guid: &[u8]) -> Status;
        fn d3d9_add_dirty_rect(
            resource: &mut D3d9Resource,
            face: u32,
            level: u32,
            full_region: bool,
            rect: &D3d9LockRect,
        ) -> Status;
        fn d3d9_add_dirty_box(
            resource: &mut D3d9Resource,
            level: u32,
            full_region: bool,
            bounds: &D3d9LockBox,
        ) -> Status;
        fn d3d9_preload(resource: &mut D3d9Resource) -> Status;
        fn d3d9_set_autogen_filter(resource: &mut D3d9Resource, filter: u32) -> Status;
        fn d3d9_get_autogen_filter(resource: &D3d9Resource) -> ValueResult;
        fn d3d9_generate_mips(resource: &mut D3d9Resource) -> Status;
        fn d3d9_autogen_capable(resource: &D3d9Resource) -> ValueResult;
        fn d3d9_format_autogen_capable(format: u32) -> ValueResult;
        fn d3d9_upload_buffer(resource: &mut D3d9Resource) -> Status;
        fn d3d9_upload_level(resource: &mut D3d9Resource, face: u32, level: u32) -> Status;
        fn d3d9_readback_level(resource: &mut D3d9Resource, face: u32, level: u32) -> Status;
        fn d3d9_register_resource_view(
            device: &mut D3d9Device,
            id: u32,
            resource: &D3d9Resource,
            face: u32,
            level: u32,
            volume: bool,
        ) -> Status;
        fn d3d9_register_implicit_targets(
            device: &mut D3d9Device,
            backbuffer: u32,
            depth: u32,
        ) -> Status;
        fn d3d9_begin_scene(device: &mut D3d9Device) -> Status;
        fn d3d9_end_scene(device: &mut D3d9Device) -> Status;
        fn d3d9_flush(device: &mut D3d9Device) -> Status;
        fn d3d9_set_render_target(
            device: &mut D3d9Device,
            index: u32,
            id: u32,
            face: u32,
            level: u32,
            volume: bool,
        ) -> Status;
        fn d3d9_set_depth_stencil(
            device: &mut D3d9Device,
            id: u32,
            face: u32,
            level: u32,
            volume: bool,
        ) -> Status;
        fn d3d9_get_render_target(device: &D3d9Device) -> ValueResult;
        fn d3d9_get_depth_stencil(device: &D3d9Device) -> ValueResult;
        fn d3d9_clear(
            device: &mut D3d9Device,
            flags: u32,
            color: u32,
            z: f32,
            stencil: u32,
            rects: &[D3d9Rect],
        ) -> Status;
        fn d3d9_draw_primitive(
            device: &mut D3d9Device,
            primitive: u32,
            start_vertex: u32,
            primitive_count: u32,
        ) -> Status;
        fn d3d9_draw_indexed_primitive(
            device: &mut D3d9Device,
            primitive: u32,
            base_vertex: i32,
            min_index: u32,
            num_vertices: u32,
            start_index: u32,
            primitive_count: u32,
        ) -> Status;
        fn d3d9_draw_primitive_up(
            device: &mut D3d9Device,
            primitive: u32,
            primitive_count: u32,
            vertices: &[u8],
            stride: u32,
        ) -> Status;
        fn d3d9_draw_indexed_primitive_up(
            device: &mut D3d9Device,
            primitive: u32,
            min_vertex: u32,
            num_vertices: u32,
            primitive_count: u32,
            indices: &[u8],
            index_format: u32,
            vertices: &[u8],
            stride: u32,
        ) -> Status;
        fn d3d9_stretch_rect(
            device: &mut D3d9Device,
            src: u32,
            src_rect: &[D3d9Rect],
            dst: u32,
            dst_rect: &[D3d9Rect],
            filter: u32,
        ) -> Status;
        fn d3d9_update_surface(
            device: &mut D3d9Device,
            source: &mut D3d9Resource,
            src_face: u32,
            src_level: u32,
            src_rect: &[D3d9Rect],
            destination: &mut D3d9Resource,
            dst_face: u32,
            dst_level: u32,
            dst_x: i32,
            dst_y: i32,
        ) -> Status;
        fn d3d9_update_texture(
            device: &mut D3d9Device,
            source: &mut D3d9Resource,
            destination: &mut D3d9Resource,
        ) -> Status;
        fn d3d9_get_render_target_data(
            device: &mut D3d9Device,
            src: u32,
            destination: &mut D3d9Resource,
            dst_face: u32,
            dst_level: u32,
        ) -> Status;
        fn d3d9_color_fill(
            device: &mut D3d9Device,
            dst: u32,
            rect: &[D3d9Rect],
            color: u32,
        ) -> Status;
        fn d3d9_create_query(device: &mut D3d9Device, kind: u32) -> ValueResult;
        fn d3d9_destroy_query(device: &mut D3d9Device, handle: u32) -> Status;
        fn d3d9_query_type(device: &D3d9Device, handle: u32) -> ValueResult;
        fn d3d9_query_data_size(kind: u32) -> ValueResult;
        fn d3d9_query_issue(device: &mut D3d9Device, handle: u32, flags: u32) -> Status;
        fn d3d9_query_get_data(
            device: &mut D3d9Device,
            handle: u32,
            data: &mut [u8],
            size: u32,
            flags: u32,
        ) -> ValueResult;
        fn d3d9_register_resource(
            device: &mut D3d9Device,
            id: u32,
            resource: &D3d9Resource,
        ) -> Status;
        fn d3d9_unregister_resource(device: &mut D3d9Device, id: u32) -> Status;
        fn d3d9_registered_resource_ids(device: &D3d9Device) -> ValuesResult;
        fn d3d9_reset(device: &mut D3d9Device, request: &D3d9ResetRequest) -> D3d9ResetResult;
        fn d3d9_test_cooperative_level(device: &mut D3d9Device) -> ValueResult;
        fn d3d9_prepare_resource_reset(
            device: &mut D3d9Device,
            id: u32,
            resource: &D3d9Resource,
        ) -> Status;
        fn d3d9_reset_ready(device: &mut D3d9Device) -> Status;
        fn d3d9_commit_resource_reset(
            device: &mut D3d9Device,
            id: u32,
            resource: &mut D3d9Resource,
        );
        fn d3d9_finish_reset(device: &mut D3d9Device) -> D3d9ResetResult;
        fn d3d9_abort_reset(device: &mut D3d9Device) -> Status;
        fn d3d9_present(
            device: &mut D3d9Device,
            src_rect: &[D3d9Rect],
            dst_rect: &[D3d9Rect],
        ) -> D3d9FrameResult;
        fn d3d9_present_readback(device: &D3d9Device) -> D3d9FrameResult;
        fn d3d9_present_handoff(
            device: &mut D3d9Device,
            src_rect: &[D3d9Rect],
            dst_rect: &[D3d9Rect],
        ) -> FrameHandoffResult;
        unsafe fn present_lease_ready(lease: usize) -> u32;
        unsafe fn release_present_lease(lease: usize);
        fn d3d9_commit_present(device: &mut D3d9Device);
        fn d3d9_backbuffer_descriptor(device: &D3d9Device) -> D3d9SurfaceDescriptorResult;
        fn d3d9_depthbuffer_descriptor(device: &D3d9Device) -> D3d9SurfaceDescriptorResult;
        fn d3d9_number_of_swap_chains(device: &D3d9Device) -> ValueResult;
        fn d3d9_display_mode(
            device: &D3d9Device,
            request: &D3d9DisplayModeRequest,
        ) -> D3d9DisplayModeResult;
        fn d3d9_effective_present(device: &D3d9Device) -> D3d9PresentParams;
        fn d3d9_device_caps(device: &D3d9Device) -> D3d9CapsResult;
        fn d3d9_adapter_caps(adapter: u32) -> D3d9CapsResult;
        fn d3d9_adapter_identifier(adapter: u32) -> D3d9AdapterIdentifierResult;
        fn d3d9_check_device_type(request: &D3d9CheckDeviceTypeRequest) -> Status;
        fn d3d9_check_device_format(request: &D3d9CheckDeviceFormatRequest) -> Status;
        fn d3d9_check_device_multisample_type(request: &D3d9CheckMultiSampleRequest)
        -> ValueResult;
        fn d3d9_check_depth_stencil_match(request: &D3d9CheckDepthStencilRequest) -> Status;
        fn d3d9_check_device_format_conversion(
            request: &D3d9CheckFormatConversionRequest,
        ) -> Status;
        fn adapter_info() -> AdapterInfoResult;
        fn set_device_dimensions(device: &mut Device, width: u32, height: u32) -> Status;
        fn set_device_format(device: &mut Device, format: u32, depth_format: u32) -> Status;
        fn initialize_gpu(
            device: &mut Device,
            width: u32,
            height: u32,
            format: u32,
            depth_format: u32,
        ) -> Status;
        fn reset_device(device: &mut Device, params: &PresentParams) -> Status;
        fn global_reset(device: &mut Device) -> ValuesResult;
        fn gpu_enabled(device: &Device) -> bool;
        fn device_info(device: &Device) -> DeviceInfoResult;
        fn set_stream(device: &mut Device, buffer_id: u32, stride: u32) -> BindingResult;
        fn set_indices(device: &mut Device, buffer_id: u32, base_vertex: u32) -> BindingResult;
        fn set_bound_texture(device: &mut Device, stage: u32, texture_id: u32) -> BindingResult;
        fn bound_texture(device: &Device, stage: u32) -> ValueResult;

        fn new_resource() -> Box<Resource>;
        fn set_resource_flags(
            resource: &mut Resource,
            usage: u32,
            pool: u32,
            depth: bool,
            gpu_target: bool,
            buffer_fvf: u32,
            index_format: u32,
            buffer_usage: u32,
            buffer_pool: u32,
        );
        fn resource_info(resource: &Resource) -> ResourceInfo;
        fn set_resource_lod(resource: &mut Resource, lod: u32) -> ValueResult;
        fn set_resource_priority(resource: &mut Resource, priority: u32) -> ValueResult;
        fn set_resource_generation(resource: &mut Resource, generation: u64);
        fn resource_generation(resource: &Resource) -> u64;
        fn set_resource_level(
            resource: &mut Resource,
            level: u32,
            width: u32,
            height: u32,
            pitch: u32,
            byte_len: u32,
            generation: u64,
        ) -> Status;
        fn resource_level(resource: &Resource, level: u32) -> ResourceLevelResult;

        fn bump_resource_generation(resource: &mut Resource) -> u64;
        fn bump_resource_level_generation(
            resource: &mut Resource,
            level: u32,
        ) -> ResourceGenerationResult;
        fn configure_texture_resource(
            resource: &mut Resource,
            width: u32,
            height: u32,
            levels: u32,
            format: u32,
        ) -> ResourceConfigurationResult;
        fn create_shader(
            device: &mut Device,
            pixel: bool,
            declaration: &[u32],
            function: &[u32],
        ) -> ShaderHandleResult;
        fn bind_shader(device: &mut Device, pixel: bool, handle: u32) -> Status;
        fn delete_shader(device: &mut Device, pixel: bool, handle: u32) -> Status;
        fn selected_shader(device: &Device, pixel: bool) -> u32;
        fn shader_stream_count(device: &Device) -> u32;
        fn draw_vertex_streams(
            device: &mut Device,
            topology: u32,
            streams: &[VertexStream],
            start_vertex: u32,
            primitive_count: u32,
            indexed: bool,
            indices: &[u8],
            index_format: u32,
            base_vertex: u32,
            min_index: u32,
            num_vertices: u32,
            start_index: u32,
        ) -> Status;
        fn set_shader_constants(
            device: &mut Device,
            pixel: bool,
            register_index: u32,
            values: &[u32],
        ) -> Status;
        fn get_shader_constants(
            device: &Device,
            pixel: bool,
            register_index: u32,
            count: u32,
        ) -> ConstantRead;
        fn shader_tokens(
            device: &Device,
            pixel: bool,
            handle: u32,
            declaration: bool,
        ) -> ValuesResult;

        fn new_storage() -> Box<Storage>;
        fn allocate_storage(storage: &mut Storage, size: u32) -> Status;
        fn storage_bytes(storage: &Storage) -> &[u8];
        fn storage_bytes_mut(storage: &mut Storage) -> &mut [u8];
        fn copy_storage(destination: &mut Storage, source: &Storage) -> Status;
        fn format_bytes(format: u32) -> u32;
        fn block_bytes(format: u32) -> u32;
        fn texture_level_layout(
            width: u32,
            height: u32,
            levels: u32,
            level: u32,
            format: u32,
        ) -> LevelLayoutResult;
        fn clear(
            device: &mut Device,
            rects: &[Rect],
            flags: u32,
            color: u32,
            z: f32,
            stencil: u32,
        ) -> Status;
        fn begin_scene(device: &mut Device) -> Status;
        fn end_scene(device: &mut Device) -> Status;
        fn set_render_state(device: &mut Device, state: u32, value: u32) -> Status;
        fn set_texture_stage_state(
            device: &mut Device,
            stage: u32,
            state: u32,
            value: u32,
        ) -> Status;
        fn set_texture(
            device: &mut Device,
            stage: u32,
            texture_id: u32,
            format: u32,
            levels: &[TextureLevel],
        ) -> Status;
        fn set_render_target(
            device: &mut Device,
            texture_id: u32,
            level: u32,
            generation: u64,
            format: u32,
            width: u32,
            height: u32,
            data: &[u8],
            depth: bool,
            depth_id: u32,
            depth_width: u32,
            depth_height: u32,
            depth_format: u32,
            change_color: bool,
        ) -> Status;
        fn read_texture(
            device: &Device,
            texture_id: u32,
            level: u32,
            generation: u64,
            out: &mut [u8],
        ) -> Status;
        fn release_texture(device: &mut Device, texture_id: u32);
        fn set_transform(device: &mut Device, kind: u32, values: &[f32]) -> Status;
        fn set_viewport(device: &mut Device, viewport: &Viewport) -> Status;
        fn get_viewport(device: &Device) -> ViewportResult;
        fn set_material(device: &mut Device, material: &Material) -> Status;
        fn get_material(device: &Device) -> MaterialResult;
        fn set_light(device: &mut Device, index: u32, light: &Light) -> Status;
        fn get_light(device: &Device, index: u32) -> LightResult;
        fn light_enable(device: &mut Device, index: u32, enabled: bool) -> Status;
        fn get_light_enable(device: &Device, index: u32) -> ValueResult;
        fn draw_primitive(
            device: &mut Device,
            topology: u32,
            fvf: u32,
            vertices: &[u8],
            stride: u32,
            start_vertex: u32,
            primitive_count: u32,
        ) -> Status;
        fn draw_indexed_primitive(
            device: &mut Device,
            topology: u32,
            fvf: u32,
            vertices: &[u8],
            stride: u32,
            indices: &[u8],
            index_format: u32,
            base_vertex: u32,
            min_index: u32,
            num_vertices: u32,
            start_index: u32,
            primitive_count: u32,
        ) -> Status;
        fn process_vertices(
            device: &mut Device,
            src: &[u8],
            src_fvf: u32,
            src_stride: u32,
            dest: &mut [u8],
            dest_fvf: u32,
            src_start: u32,
            dest_index: u32,
            count: u32,
        ) -> Status;
        fn read_pixels_len(device: &Device, out: &mut u64) -> Status;
        fn read_pixels(device: &Device, out: &mut [u8]) -> Status;
        fn write_pixels(
            device: &mut Device,
            x: u32,
            y: u32,
            width: u32,
            height: u32,
            rgba: &[u8],
        ) -> Status;
        fn present(device: &mut Device) -> Status;
        fn present_handoff(device: &Device, bpp: u32) -> FrameHandoffResult;
        fn scene_boundary(device: &mut Device, mode: u32) -> Status;

        fn new_device7_state() -> Box<Device7State>;
        fn d3d7_get_render_state(state: &Device7State, index: u32) -> ValueResult;
        fn d3d7_set_render_state(state: &mut Device7State, index: u32, value: u32) -> Status;
        fn d3d7_render_state_was_set(state: &Device7State, index: u32) -> bool;
        fn d3d7_get_texture_stage_state(
            state: &Device7State,
            stage: u32,
            index: u32,
        ) -> ValueResult;
        fn d3d7_set_texture_stage_state(
            state: &mut Device7State,
            stage: u32,
            index: u32,
            value: u32,
        ) -> Status;
        fn d3d7_texture_stage_state_was_set(state: &Device7State, stage: u32, index: u32) -> bool;
        fn d3d7_get_transform(state: &Device7State, index: u32) -> FloatValuesResult;
        fn d3d7_set_transform(state: &mut Device7State, index: u32, values: &[f32]) -> Status;
        fn d3d7_transform_was_set(state: &Device7State, index: u32) -> bool;
        fn d3d7_get_viewport(state: &Device7State) -> ViewportResult;
        fn d3d7_set_viewport(state: &mut Device7State, viewport: &Viewport) -> Status;
        fn d3d7_viewport_was_set(state: &Device7State) -> bool;
        fn d3d7_get_material(state: &Device7State) -> ValuesResult;
        fn d3d7_set_material(state: &mut Device7State, values: &[u32]) -> Status;
        fn d3d7_material_was_set(state: &Device7State) -> bool;
        fn d3d7_get_light(state: &Device7State, index: u32) -> ValuesResult;
        fn d3d7_set_light(state: &mut Device7State, index: u32, values: &[u32]) -> Status;
        fn d3d7_get_light_enable(state: &Device7State, index: u32) -> ValueResult;
        fn d3d7_set_light_enable(state: &mut Device7State, index: u32, value: u32) -> Status;
        fn d3d7_get_texture(state: &Device7State, stage: u32) -> ValueResult;
        fn d3d7_set_texture(
            state: &mut Device7State,
            stage: u32,
            object_id: u32,
        ) -> TextureSetResult;
        fn d3d7_get_render_target(state: &Device7State) -> u32;
        fn d3d7_set_render_target(state: &mut Device7State, object_id: u32);
        fn d3d7_recording(state: &Device7State) -> bool;
        fn d3d7_in_scene(state: &Device7State) -> bool;
        fn d3d7_set_in_scene(state: &mut Device7State, value: bool);
        fn d3d7_target_dirty(state: &Device7State) -> bool;
        fn d3d7_set_target_dirty(state: &mut Device7State, value: bool);
        fn d3d7_begin_state_block(state: &mut Device7State) -> Status;
        fn d3d7_end_state_block(state: &mut Device7State) -> HandleResult;
        fn d3d7_apply_state_block(state: &mut Device7State, handle: u32) -> ApplyStateBlockResult;
        fn d3d7_delete_state_block(state: &mut Device7State, handle: u32) -> ValuesResult;
        fn d3d7_release_refs(state: &mut Device7State) -> ValuesResult;
        fn d3d7_state_block_render_state_was_set(
            state: &Device7State,
            handle: u32,
            index: u32,
        ) -> bool;
        fn d3d7_state_block_texture_stage_state_was_set(
            state: &Device7State,
            handle: u32,
            stage: u32,
            index: u32,
        ) -> bool;
        fn d3d7_state_block_transform_was_set(
            state: &Device7State,
            handle: u32,
            index: u32,
        ) -> bool;
        fn d3d7_state_block_viewport_was_set(state: &Device7State, handle: u32) -> bool;
        fn d3d7_state_block_material_was_set(state: &Device7State, handle: u32) -> bool;
        fn d3d7_state_block_light_was_set(state: &Device7State, handle: u32, index: u32) -> bool;
        fn d3d7_state_block_light_enable_was_set(
            state: &Device7State,
            handle: u32,
            index: u32,
        ) -> bool;
        fn d3d7_state_block_texture_was_set(state: &Device7State, handle: u32, stage: u32) -> bool;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum D3d7TranslationKind {
        Forward = 0,
        Ignore = 1,
        Invalid = 2,
    }
    struct D3d7RenderStateTranslation {
        kind: D3d7TranslationKind,
        state: u32,
    }
    struct D3d7TransformTranslation {
        valid: bool,
        state: u32,
    }
    struct D3d7TextureStageTranslation {
        count: i32,
        first: u32,
        second: u32,
    }

    extern "Rust" {
        fn translate_render_state(state: u32, value: u32) -> D3d7RenderStateTranslation;
        fn translate_transform(state: u32) -> D3d7TransformTranslation;
        fn translate_texture_stage_state(state: u32) -> D3d7TextureStageTranslation;
    }

    #[derive(Clone, Copy, Debug)]
    struct PresentParams {
        back_buffer_width: u32,
        back_buffer_height: u32,
        back_buffer_format: u32,
        back_buffer_count: u32,
        multisample_type: u32,
        swap_effect: u32,
        device_window: u32,
        windowed: u32,
        enable_auto_depth_stencil: u32,
        auto_depth_stencil_format: u32,
        flags: u32,
        fullscreen_refresh_rate: u32,
        fullscreen_presentation_interval: u32,
    }

    struct LevelLayout {
        width: u32,
        height: u32,
        pitch: u32,
        size: u32,
        levels: u32,
    }

    struct LevelLayoutResult {
        status: Status,
        layout: LevelLayout,
    }

    struct ResourceInfo {
        usage: u32,
        pool: u32,
        lod: u32,
        priority: u32,
        level_count: u32,
        depth: bool,
        gpu_target: bool,
        buffer_fvf: u32,
        index_format: u32,
        buffer_usage: u32,
        buffer_pool: u32,
    }
    struct ResourceLevel {
        width: u32,
        height: u32,
        pitch: u32,
        byte_len: u32,
        generation: u64,
    }
    struct ResourceLevelResult {
        status: Status,
        level: ResourceLevel,
    }
    struct ResourceConfigurationResult {
        status: Status,
        level_count: u32,
        levels: Vec<ResourceLevel>,
    }
    struct ResourceGenerationResult {
        status: Status,
        generation: u64,
    }
    struct BindingResult {
        status: Status,
        old_id: u32,
        new_id: u32,
        old_value: u32,
        new_value: u32,
    }
    struct DeviceInfo {
        width: u32,
        height: u32,
        format: u32,
        depth_format: u32,
        stream_vb: u32,
        stream_stride: u32,
        index_buffer: u32,
        base_vertex: u32,
    }
    struct DeviceInfoResult {
        status: Status,
        info: DeviceInfo,
    }
}

use crate::backend::native_present::{present_lease_ready, release_present_lease};
use crate::bridge_render::{
    adapter_info, begin_scene, clear, draw_indexed_primitive, draw_primitive, end_scene, get_light,
    get_light_enable, get_material, get_viewport, light_enable, present, present_handoff,
    process_vertices, read_pixels, read_pixels_len, read_texture, release_texture, scene_boundary,
    set_light, set_material, set_render_state, set_render_target, set_texture,
    set_texture_stage_state, set_transform, set_viewport, write_pixels,
};
use crate::d3d8::{
    resource::{D3d8LevelLayout, format_bytes as cpu_format_bytes, level_layout},
    shader::{Declaration, Program},
    state::DeviceState,
};
use crate::d3d9::d3dx_effect::{
    D3dxEffect, D3dxEffectPool, d3dx_effect_batch_active, d3dx_effect_batch_finish,
    d3dx_effect_begin, d3dx_effect_begin_pass, d3dx_effect_commit, d3dx_effect_create,
    d3dx_effect_current_technique, d3dx_effect_desc, d3dx_effect_end, d3dx_effect_end_pass,
    d3dx_effect_enum, d3dx_effect_find, d3dx_effect_find_next_valid, d3dx_effect_flags,
    d3dx_effect_get, d3dx_effect_is_parameter_used, d3dx_effect_join_pool, d3dx_effect_on_lost,
    d3dx_effect_on_reset, d3dx_effect_param_desc, d3dx_effect_param_name,
    d3dx_effect_param_semantic, d3dx_effect_pass_desc, d3dx_effect_pass_shader,
    d3dx_effect_pool_id, d3dx_effect_pool_id_of, d3dx_effect_pool_texture_ref_counts,
    d3dx_effect_pool_texture_refs, d3dx_effect_prepare_shaders, d3dx_effect_recycle,
    d3dx_effect_release, d3dx_effect_saved_bindings, d3dx_effect_set, d3dx_effect_set_raw,
    d3dx_effect_set_state_manager, d3dx_effect_set_technique, d3dx_effect_set_texture,
    d3dx_effect_shutdown, d3dx_effect_technique_desc, d3dx_effect_texture,
    d3dx_effect_texture_ref_counts, d3dx_effect_texture_refs, d3dx_effect_texture_shared,
    d3dx_effect_validate_technique, new_d3dx_effect, new_d3dx_effect_pool,
};
use crate::d3d9::{
    D3d9Device, D3d9Resource, D3dxSurfaceData, d3d9_abort_reset, d3d9_adapter_caps,
    d3d9_adapter_identifier, d3d9_add_dirty_box, d3d9_add_dirty_rect, d3d9_apply_state_block,
    d3d9_autogen_capable, d3d9_available_texture_mem, d3d9_backbuffer_descriptor,
    d3d9_begin_state_block, d3d9_capture_state_block, d3d9_check_depth_stencil_match,
    d3d9_check_device_format, d3d9_check_device_format_conversion,
    d3d9_check_device_multisample_type, d3d9_check_device_type, d3d9_commit_present,
    d3d9_commit_resource_reset, d3d9_create_buffer, d3d9_create_device,
    d3d9_create_fvf_declaration, d3d9_create_shader, d3d9_create_state_block, d3d9_create_surface,
    d3d9_create_texture, d3d9_create_vertex_declaration, d3d9_creation_parameters,
    d3d9_current_bindings, d3d9_d3dx_create_texture, d3d9_d3dx_image_info, d3d9_d3dx_save_surface,
    d3d9_d3dx_save_texture, d3d9_d3dx_surface_read, d3d9_d3dx_surface_store,
    d3d9_declaration_elements, d3d9_depthbuffer_descriptor, d3d9_destroy_declaration,
    d3d9_destroy_shader, d3d9_destroy_state_block, d3d9_device_caps, d3d9_display_mode,
    d3d9_effective_present, d3d9_end_state_block, d3d9_finish_reset, d3d9_format_autogen_capable,
    d3d9_free_private_data, d3d9_generate_mips, d3d9_get_autogen_filter, d3d9_get_clip_plane,
    d3d9_get_fvf, d3d9_get_indices, d3d9_get_light, d3d9_get_light_enable, d3d9_get_material,
    d3d9_get_private_data, d3d9_get_render_state, d3d9_get_sampler_state, d3d9_get_scissor,
    d3d9_get_shader, d3d9_get_shader_constants_b, d3d9_get_shader_constants_f,
    d3d9_get_shader_constants_i, d3d9_get_stream_freq, d3d9_get_stream_source, d3d9_get_texture,
    d3d9_get_texture_stage_state, d3d9_get_transform, d3d9_get_vertex_declaration,
    d3d9_get_viewport, d3d9_level_bytes, d3d9_level_lock_commit, d3d9_level_lock_prepare,
    d3d9_level_unlock, d3d9_lod, d3d9_multiply_transform, d3d9_note_binding_refresh,
    d3d9_number_of_swap_chains, d3d9_preload,
    d3d9_prepare_resource_reset, d3d9_present, d3d9_present_handoff, d3d9_present_readback,
    d3d9_priority, d3d9_readback_level, d3d9_register_resource, d3d9_registered_resource_ids,
    d3d9_reset, d3d9_reset_ready, d3d9_resource_type, d3d9_set_autogen_filter, d3d9_set_clip_plane,
    d3d9_set_fvf_binding, d3d9_set_indices, d3d9_set_light, d3d9_set_light_enable, d3d9_set_lod,
    d3d9_set_material, d3d9_set_priority, d3d9_set_private_data, d3d9_set_render_state,
    d3d9_set_sampler_state, d3d9_set_scissor, d3d9_set_shader, d3d9_set_shader_constants_b,
    d3d9_set_shader_constants_f, d3d9_set_shader_constants_i, d3d9_set_stream_freq,
    d3d9_set_stream_source, d3d9_set_texture, d3d9_set_texture_stage_state, d3d9_set_transform,
    d3d9_set_vertex_declaration, d3d9_set_viewport, d3d9_shader_function, d3d9_shader_size,
    d3d9_state_block_bindings, d3d9_surface_descriptor, d3d9_test_cooperative_level,
    d3d9_texture_descriptor, d3d9_texture_level_descriptor, d3d9_texture_volume_descriptor,
    d3d9_unregister_resource, d3d9_upload_buffer, d3d9_upload_level, new_d3d9_device,
    new_d3d9_resource,
};
use crate::d3d9::{
    D3dxConstantTable, d3dx_constant_table_apply, d3dx_constant_table_constant,
    d3dx_constant_table_create, d3dx_constant_table_defaults, d3dx_constant_table_desc,
    d3dx_constant_table_element, d3dx_constant_table_find, d3dx_constant_table_info,
    d3dx_constant_table_input_size, d3dx_constant_table_sampler, d3dx_matrix_inverse,
    d3dx_matrix_multiply, d3dx_matrix_transpose, d3dx_shader_input_semantics, d3dx_shader_profiles,
    d3dx_shader_version, new_d3dx_constant_table,
};
use crate::d3d9::{
    d3d9_begin_scene, d3d9_clear, d3d9_color_fill, d3d9_create_query, d3d9_destroy_query,
    d3d9_draw_indexed_primitive, d3d9_draw_indexed_primitive_up, d3d9_draw_primitive,
    d3d9_draw_primitive_up, d3d9_end_scene, d3d9_flush, d3d9_get_depth_stencil,
    d3d9_get_render_target, d3d9_get_render_target_data, d3d9_query_data_size, d3d9_query_get_data,
    d3d9_query_issue, d3d9_query_type, d3d9_register_implicit_targets, d3d9_register_resource_view,
    d3d9_set_depth_stencil, d3d9_set_render_target, d3d9_stretch_rect, d3d9_update_surface,
    d3d9_update_texture,
};
use crate::guest::buffer::{
    GuestBuffer, GuestBufferBudget, guest_buffer_descriptor, guest_buffer_lock,
    guest_buffer_prepare_create, guest_buffer_release, guest_buffer_sync, guest_buffer_unlock,
    guest_publish_interface, new_guest_buffer, new_guest_buffer_budget,
};
use crate::guest::hresult::guest_status_hresult;
use crate::guest::texture::{
    GuestTexture, guest_rect_lock, guest_rect_prepare, guest_rect_prepare_unlock,
    guest_rect_unlock, guest_surface_desc, guest_surface_prepare_create,
    guest_texture_prepare_create, guest_texture_release, new_guest_texture,
};
use crate::{RenderError, RenderErrorKind, d3d8::device::Device as RenderDevice};
use std::collections::BTreeMap;

pub use ffi::{
    AdapterInfo, AdapterInfoResult, ApplyStateBlockResult, BindingResult, ConstantRead,
    D3d7RenderStateTranslation, D3d7TextureStageTranslation, D3d7TransformTranslation,
    D3d7TranslationKind, D3d9AdapterIdentifierResult, D3d9BindingSnapshot, D3d9Bindings,
    D3d9BindingsResult,
    D3d9BlockBindingsResult, D3d9BufferKind, D3d9BufferRequest, D3d9BufferUnlockResult,
    D3d9CapsResult, D3d9CheckDepthStencilRequest, D3d9CheckDeviceFormatRequest,
    D3d9CheckDeviceTypeRequest, D3d9CheckFormatConversionRequest, D3d9CheckMultiSampleRequest,
    D3d9CreateResult, D3d9CreationParameters, D3d9CreationParametersResult, D3d9DeclarationResult,
    D3d9DeviceRequest, D3d9DisplayMode, D3d9DisplayModeRequest, D3d9DisplayModeResult,
    D3d9FrameResult, D3d9GuestBufferCreateResult, D3d9GuestRectRequest, D3d9GuestRectRequestResult,
    D3d9GuestSurfaceCreateResult, D3d9GuestTextureCreateResult, D3d9LevelDescriptor,
    D3d9LevelDescriptorResult, D3d9LevelLockRequest, D3d9LevelLockResult, D3d9LightResult,
    D3d9LockBox, D3d9LockRect, D3d9PresentParams, D3d9PrivateDataResult, D3d9Rect,
    D3d9ResetRequest, D3d9ResetResult, D3d9ScissorResult, D3d9StreamResult, D3d9SurfaceDescriptor,
    D3d9SurfaceDescriptorResult, D3d9SurfaceRequest, D3d9TextureCreateResult,
    D3d9TextureDescriptor, D3d9TextureDescriptorResult, D3d9TextureKind, D3d9TextureRequest,
    D3d9VertexElement, D3dxBytesResult, D3dxConstantDesc, D3dxConstantDescResult,
    D3dxConstantTableDesc, D3dxConstantTableResult, D3dxCreateRequest, D3dxCreateResult,
    D3dxEffectCommand, D3dxEffectCommandKind, D3dxEffectCreateResult, D3dxEffectDesc,
    D3dxEffectDescResult, D3dxEffectParamDesc, D3dxEffectParamDescResult, D3dxEffectPassDesc,
    D3dxEffectPassDescResult, D3dxEffectRunResult, D3dxEffectShadersResult,
    D3dxEffectTechniqueDesc, D3dxEffectTechniqueDescResult, D3dxImageInfo, D3dxImageInfoResult,
    D3dxMatrixInverseResult, D3dxMatrixResult, D3dxProfilesResult, D3dxSemantic,
    D3dxSemanticsResult, D3dxSurfaceReadResult, D3dxTextureRefCounts, DeviceInfo,
    DeviceInfoResult, FloatValuesResult, FrameHandoff, FrameHandoffResult, HandleResult,
    LevelLayout, LevelLayoutResult, Light, LightResult, Material, MaterialResult, PresentParams,
    Rect, ResourceConfigurationResult, ResourceGenerationResult, ResourceInfo, ResourceLevel,
    ResourceLevelResult, ShaderHandleResult,
    StateBlockError, Status, StatusCode, TextureBindings, TextureLevel, TextureSetResult,
    ValueResult, ValuesResult, Viewport, ViewportResult,
};

pub struct Device {
    pub(crate) gpu: Option<RenderDevice>,
    pub(crate) cpu_state: DeviceState,
    pub(crate) viewport_was_set: bool,
    target_width: u32,
    target_height: u32,
    target_format: u32,
    depth_format: u32,
    stream_vb: u32,
    stream_stride: u32,
    index_buffer: u32,
    base_vertex: u32,
    bound_textures: [u32; 8],
    next_shader: u32,
    shaders: BTreeMap<u32, ShaderRecord>,
    vertex_shader: u32,
    vertex_fvf: u32,
    pixel_shader: u32,
    vertex_constants: Vec<u32>,
    pixel_constants: Vec<u32>,
}

struct ShaderRecord {
    pixel: bool,
    declaration: Vec<u32>,
    function: Vec<u32>,
    parsed_declaration: Option<Declaration>,
    parsed_program: Program,
}

#[derive(Clone)]
struct Device7Data {
    render_state: [u32; 256],
    render_state_set: [bool; 256],
    texture_stage_state: [[u32; 256]; 8],
    texture_stage_state_set: [[bool; 256]; 8],
    transforms: [[f32; 16]; 256],
    transform_set: [bool; 256],
    viewport: Viewport,
    viewport_set: bool,
    material: [u32; 17],
    material_set: bool,
    lights: [[u32; 26]; 8],
    light_set: [bool; 8],
    light_enable: [u32; 8],
    light_enable_set: [bool; 8],
    textures: [u32; 8],
    texture_set: [bool; 8],
    render_target: u32,
    in_scene: bool,
    target_dirty: bool,
}

impl Default for Device7Data {
    fn default() -> Self {
        let mut data = Self {
            render_state: [0; 256],
            render_state_set: [false; 256],
            texture_stage_state: [[0; 256]; 8],
            texture_stage_state_set: [[false; 256]; 8],
            transforms: [[0.0; 16]; 256],
            transform_set: [false; 256],
            viewport: Viewport {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                min_z: 0.0,
                max_z: 1.0,
            },
            viewport_set: false,
            material: [0; 17],
            material_set: false,
            lights: [[0; 26]; 8],
            light_set: [false; 8],
            light_enable: [0; 8],
            light_enable_set: [false; 8],
            textures: [0; 8],
            texture_set: [false; 8],
            render_target: 0,
            in_scene: false,
            target_dirty: true,
        };
        for index in 0..256 {
            data.render_state[index] = d3d7_render_default(index);
            data.transforms[index][0] = 1.0;
            data.transforms[index][5] = 1.0;
            data.transforms[index][10] = 1.0;
            data.transforms[index][15] = 1.0;
        }
        for stage in 0..8 {
            for index in 0..256 {
                data.texture_stage_state[stage][index] = d3d7_texture_default(stage, index);
            }
        }
        data.material[..4].fill(0x3f80_0000);
        data
    }
}

pub struct Device7State {
    live: Device7Data,
    recording: Option<Device7Data>,
    state_blocks: BTreeMap<u32, Device7Data>,
    next_state_block: u32,
}

impl Default for Device7State {
    fn default() -> Self {
        Self {
            live: Device7Data::default(),
            recording: None,
            state_blocks: BTreeMap::new(),
            next_state_block: 1,
        }
    }
}

fn d3d7_render_default(index: usize) -> u32 {
    match index {
        8 => 3,
        9 => 2,
        14 | 16 => 1,
        19 => 2,
        20 => 1,
        22 => 3,
        23 => 4,
        25 | 56 => 8,
        37 | 38 | 154 | 158 | 164 => 0x3f80_0000,
        58 | 59 | 60 | 162 => u32::MAX,
        136 | 137 | 141 | 142 | 161 => 1,
        146 => 1,
        147 | 148 => 2,
        168 => 0xf,
        171 => 1,
        _ => 0,
    }
}

fn d3d7_texture_default(stage: usize, index: usize) -> u32 {
    match index {
        1 => {
            if stage == 0 {
                4
            } else {
                1
            }
        }
        2 | 5 => 2,
        3 | 6 => 1,
        4 => {
            if stage == 0 {
                2
            } else {
                1
            }
        }
        12..=14 => 1,
        16 | 17 => 2,
        _ => 0,
    }
}

fn state_write_target(state: &mut Device7State) -> &mut Device7Data {
    if state.recording.is_some() {
        state.recording.as_mut().unwrap()
    } else {
        &mut state.live
    }
}

pub struct Storage {
    storage: Option<crate::d3d8::resource::CpuStorage>,
}

#[derive(Clone, Copy, Default)]
struct ResourceLevelData {
    width: u32,
    height: u32,
    pitch: u32,
    byte_len: u32,
    generation: u64,
}

#[derive(Clone, Copy, Default)]
struct ResourceMeta {
    usage: u32,
    pool: u32,
    lod: u32,
    priority: u32,
    level_count: u32,
    depth: bool,
    gpu_target: bool,
    buffer_fvf: u32,
    index_format: u32,
    buffer_usage: u32,
    buffer_pool: u32,
}

pub struct Resource {
    info: ResourceMeta,
    generation: u64,
    levels: BTreeMap<u32, ResourceLevelData>,
}

impl Default for Resource {
    fn default() -> Self {
        Self {
            info: ResourceMeta::default(),
            generation: 0,
            levels: BTreeMap::new(),
        }
    }
}

pub(crate) fn status(code: StatusCode, message: impl Into<String>) -> Status {
    Status {
        code,
        message: message.into(),
        state_block_error: StateBlockError::None,
    }
}

fn state_block_status(code: StatusCode, message: &'static str, error: StateBlockError) -> Status {
    Status {
        code,
        message: message.into(),
        state_block_error: error,
    }
}

pub(crate) fn status_from_error(error: RenderError) -> Status {
    let code = match error.kind {
        RenderErrorKind::InvalidArgument => StatusCode::InvalidArgument,
        RenderErrorKind::InvalidData => StatusCode::InvalidData,
        RenderErrorKind::Unsupported => StatusCode::Unsupported,
        RenderErrorKind::OutOfMemory => StatusCode::OutOfMemory,
    };
    status(code, error.to_string())
}

impl Default for Device {
    fn default() -> Self {
        Self {
            gpu: None,
            cpu_state: DeviceState::new(0, 0),
            viewport_was_set: false,
            target_width: 0,
            target_height: 0,
            target_format: 0,
            depth_format: 0,
            stream_vb: 0,
            stream_stride: 0,
            index_buffer: 0,
            base_vertex: 0,
            bound_textures: [0; 8],
            next_shader: 0xf000_0001,
            shaders: BTreeMap::new(),
            vertex_shader: 0,
            vertex_fvf: 0,
            pixel_shader: 0,
            vertex_constants: vec![0; 96 * 4],
            pixel_constants: vec![0; 8 * 4],
        }
    }
}

pub fn new_device() -> Box<Device> {
    Box::default()
}

impl Device {
    pub(crate) fn state(&self) -> &DeviceState {
        self.gpu
            .as_ref()
            .map_or(&self.cpu_state, |renderer| &renderer.state)
    }
    pub(crate) fn state_mut(&mut self) -> &mut DeviceState {
        self.gpu
            .as_mut()
            .map_or(&mut self.cpu_state, |renderer| &mut renderer.state)
    }
}

pub fn set_device_dimensions(device: &mut Device, width: u32, height: u32) -> Status {
    if width == 0 || height == 0 {
        return status(
            StatusCode::InvalidArgument,
            "CreateDevice: zero backbuffer dimensions",
        );
    }
    device.target_width = width;
    device.target_height = height;
    device.state_mut().set_target_size(width, height);
    if !device.viewport_was_set {
        device.state_mut().viewport = crate::d3d8::state::Viewport::full(width, height);
    }
    status(StatusCode::Ok, "")
}

pub fn set_device_format(device: &mut Device, format: u32, depth_format: u32) -> Status {
    device.target_format = format;
    device.depth_format = depth_format;
    match device
        .state_mut()
        .set_render_state(7, u32::from(depth_format != 0))
    {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => status_from_error(error),
    }
}

pub fn initialize_gpu(
    device: &mut Device,
    width: u32,
    height: u32,
    format: u32,
    depth_format: u32,
) -> Status {
    if device.gpu.is_some() {
        return status(
            StatusCode::InvalidArgument,
            "initialize_gpu: device already initialized",
        );
    }
    if width == 0 || height == 0 {
        return status(
            StatusCode::InvalidArgument,
            "initialize_gpu: dimensions must be nonzero",
        );
    }
    let gpu = match crate::kit::context() {
        Ok(gpu) => gpu,
        Err(error) => return status(StatusCode::Backend, error.to_string()),
    };
    let renderer = match RenderDevice::new(gpu, width, height, format, depth_format) {
        Ok(renderer) => renderer,
        Err(error) => return status_from_error(error),
    };
    device.target_width = width;
    device.target_height = height;
    device.target_format = format;
    device.depth_format = depth_format;
    let mut state = device.cpu_state.clone();
    if let Err(error) = state.set_render_state(7, u32::from(depth_format != 0)) {
        return status_from_error(error);
    }
    state.set_target_size(width, height);
    if !device.viewport_was_set {
        state.viewport = crate::d3d8::state::Viewport::full(width, height);
    }
    device.cpu_state = state.clone();
    device.gpu = Some(renderer);
    for (handle, shader) in &device.shaders {
        let renderer = device.gpu.as_mut().unwrap();
        renderer.shaders.insert(
            *handle,
            (
                shader.parsed_declaration.clone(),
                shader.parsed_program.clone(),
            ),
        );
    }
    if let Some(renderer) = device.gpu.as_mut() {
        renderer.state = state;
        renderer.vertex_shader = device.vertex_shader;
        renderer.pixel_shader = device.pixel_shader;
        for (register, target) in renderer.shader_uniform.vc.iter_mut().enumerate() {
            for component in 0..4 {
                target[component] =
                    f32::from_bits(device.vertex_constants[register * 4 + component]);
            }
        }
        for (register, target) in renderer.shader_uniform.pc.iter_mut().enumerate() {
            for component in 0..4 {
                target[component] =
                    f32::from_bits(device.pixel_constants[register * 4 + component]);
            }
        }
    }
    status(StatusCode::Ok, "")
}

pub fn reset_device(device: &mut Device, params: &PresentParams) -> Status {
    let depth_format = match validate_present_params(params) {
        Ok(value) => value,
        Err(error) => return status_from_error(error),
    };
    if let Some(renderer) = device.gpu.as_mut() {
        if let Err(error) = renderer.reset(
            params.back_buffer_width,
            params.back_buffer_height,
            params.back_buffer_format,
            depth_format,
        ) {
            return status_from_error(error);
        }
    }
    let mut state = DeviceState::new(params.back_buffer_width, params.back_buffer_height);
    if let Err(error) = state.set_render_state(7, u32::from(depth_format != 0)) {
        return status_from_error(error);
    }
    if let Some(renderer) = device.gpu.as_ref() {
        state = renderer.state.clone();
    }
    device.cpu_state = state;
    device.target_width = params.back_buffer_width;
    device.target_height = params.back_buffer_height;
    device.target_format = params.back_buffer_format;
    device.depth_format = depth_format;
    device.viewport_was_set = false;
    device.vertex_shader = 0;
    device.vertex_fvf = 0;
    device.pixel_shader = 0;
    device.stream_vb = 0;
    device.stream_stride = 0;
    device.index_buffer = 0;
    device.base_vertex = 0;
    device.bound_textures.fill(0);
    device.vertex_constants.fill(0);
    device.pixel_constants.fill(0);
    status(StatusCode::Ok, "")
}

fn validate_present_params(params: &PresentParams) -> Result<u32, RenderError> {
    use crate::d3d8::format::{self, DepthFormat};
    const SWAPEFFECT_DISCARD: u32 = 1;
    const SWAPEFFECT_FLIP: u32 = 2;
    const SWAPEFFECT_COPY_VSYNC: u32 = 4;
    const PRESENT_INTERVAL_IMMEDIATE: u32 = 0x8000_0000;
    let invalid = |what: &str| RenderError::invalid("reset", what);
    if params.back_buffer_width == 0 || params.back_buffer_height == 0 {
        return Err(invalid("zero backbuffer dimension"));
    }
    if u64::from(params.back_buffer_width) * u64::from(params.back_buffer_height) * 4
        > u64::from(u32::MAX)
    {
        return Err(invalid("backbuffer size overflow"));
    }
    if params.back_buffer_format != format::D3DFMT_A8R8G8B8
        && params.back_buffer_format != format::D3DFMT_X8R8G8B8
    {
        return Err(invalid("unsupported backbuffer format"));
    }
    if params.back_buffer_count > 1 || params.multisample_type != 0 {
        return Err(invalid("unsupported backbuffer count or multisample type"));
    }
    let swap_ok = params.swap_effect == SWAPEFFECT_DISCARD
        || params.swap_effect == SWAPEFFECT_COPY_VSYNC
        || (params.swap_effect == SWAPEFFECT_FLIP && params.windowed == 0);
    if !swap_ok {
        return Err(invalid("unsupported swap effect"));
    }
    let interval_ok = params.fullscreen_presentation_interval == 0
        || params.fullscreen_presentation_interval == PRESENT_INTERVAL_IMMEDIATE;
    if params.flags & !1 != 0 || params.fullscreen_refresh_rate != 0 || !interval_ok {
        return Err(invalid("unsupported present flags/refresh/interval"));
    }
    let depth = DepthFormat::from_d3dformat(params.auto_depth_stencil_format)
        .map_err(|_| invalid("unsupported autodepth format"))?;
    if params.enable_auto_depth_stencil == 0 {
        return Ok(0);
    }
    match depth {
        Some(value) => Ok(value.d3dformat()),
        None => Err(invalid("autodepth enabled without a depth format")),
    }
}

pub fn global_reset(device: &mut Device) -> ValuesResult {
    let mut released = Vec::with_capacity(10);
    released.push(device.stream_vb);
    released.push(device.index_buffer);
    released.extend(device.bound_textures);
    device.shaders.clear();
    device.next_shader = 0xf000_0001;
    device.vertex_shader = 0;
    device.vertex_fvf = 0;
    device.pixel_shader = 0;
    device.vertex_constants.fill(0);
    device.pixel_constants.fill(0);
    device.stream_vb = 0;
    device.stream_stride = 0;
    device.index_buffer = 0;
    device.base_vertex = 0;
    device.bound_textures.fill(0);
    if let Some(renderer) = device.gpu.as_mut() {
        renderer.shaders.clear();
        renderer.vertex_shader = 0;
        renderer.pixel_shader = 0;
        renderer.shader_uniform = Default::default();
    }
    ValuesResult {
        status: status(StatusCode::Ok, ""),
        values: released,
    }
}

pub fn gpu_enabled(device: &Device) -> bool {
    device.gpu.is_some()
}

pub fn translate_render_state(state: u32, value: u32) -> D3d7RenderStateTranslation {
    crate::bridge_translate::translate_render_state(state, value)
}
pub fn translate_transform(state: u32) -> D3d7TransformTranslation {
    crate::bridge_translate::translate_transform(state)
}
pub fn translate_texture_stage_state(state: u32) -> D3d7TextureStageTranslation {
    crate::bridge_translate::translate_texture_stage_state(state)
}

pub fn device_info(device: &Device) -> DeviceInfoResult {
    DeviceInfoResult {
        status: status(StatusCode::Ok, ""),
        info: DeviceInfo {
            width: device.target_width,
            height: device.target_height,
            format: device.target_format,
            depth_format: device.depth_format,
            stream_vb: device.stream_vb,
            stream_stride: device.stream_stride,
            index_buffer: device.index_buffer,
            base_vertex: device.base_vertex,
        },
    }
}

pub fn set_stream(device: &mut Device, buffer_id: u32, stride: u32) -> BindingResult {
    let result = BindingResult {
        status: status(StatusCode::Ok, ""),
        old_id: device.stream_vb,
        new_id: buffer_id,
        old_value: device.stream_stride,
        new_value: stride,
    };
    device.stream_vb = buffer_id;
    device.stream_stride = stride;
    result
}

pub fn set_indices(device: &mut Device, buffer_id: u32, base_vertex: u32) -> BindingResult {
    let result = BindingResult {
        status: status(StatusCode::Ok, ""),
        old_id: device.index_buffer,
        new_id: buffer_id,
        old_value: device.base_vertex,
        new_value: base_vertex,
    };
    device.index_buffer = buffer_id;
    device.base_vertex = base_vertex;
    result
}

pub fn set_bound_texture(device: &mut Device, stage: u32, texture_id: u32) -> BindingResult {
    let Some(current) = device.bound_textures.get_mut(stage as usize) else {
        return BindingResult {
            status: status(
                StatusCode::InvalidArgument,
                "SetTexture: stage out of range",
            ),
            old_id: 0,
            new_id: texture_id,
            old_value: 0,
            new_value: 0,
        };
    };
    let old_id = *current;
    *current = texture_id;
    BindingResult {
        status: status(StatusCode::Ok, ""),
        old_id,
        new_id: texture_id,
        old_value: 0,
        new_value: 0,
    }
}

pub fn bound_texture(device: &Device, stage: u32) -> ValueResult {
    device
        .bound_textures
        .get(stage as usize)
        .copied()
        .map(value_result)
        .unwrap_or_else(|| invalid_value("GetTexture: stage out of range"))
}

pub fn new_resource() -> Box<Resource> {
    Box::default()
}
pub fn set_resource_flags(
    resource: &mut Resource,
    usage: u32,
    pool: u32,
    depth: bool,
    gpu_target: bool,
    buffer_fvf: u32,
    index_format: u32,
    buffer_usage: u32,
    buffer_pool: u32,
) {
    resource.info.usage = usage;
    resource.info.pool = pool;
    resource.info.depth = depth;
    resource.info.gpu_target = gpu_target;
    resource.info.buffer_fvf = buffer_fvf;
    resource.info.index_format = index_format;
    resource.info.buffer_usage = buffer_usage;
    resource.info.buffer_pool = buffer_pool;
}
pub fn resource_info(resource: &Resource) -> ResourceInfo {
    ResourceInfo {
        usage: resource.info.usage,
        pool: resource.info.pool,
        lod: resource.info.lod,
        priority: resource.info.priority,
        level_count: resource.info.level_count,
        depth: resource.info.depth,
        gpu_target: resource.info.gpu_target,
        buffer_fvf: resource.info.buffer_fvf,
        index_format: resource.info.index_format,
        buffer_usage: resource.info.buffer_usage,
        buffer_pool: resource.info.buffer_pool,
    }
}
pub fn set_resource_lod(resource: &mut Resource, lod: u32) -> ValueResult {
    let previous = resource.info.lod;
    let maximum = resource.info.level_count.saturating_sub(1);
    resource.info.lod = lod.min(maximum);
    value_result(previous)
}
pub fn set_resource_priority(resource: &mut Resource, priority: u32) -> ValueResult {
    let previous = resource.info.priority;
    resource.info.priority = priority;
    value_result(previous)
}
pub fn set_resource_generation(resource: &mut Resource, generation: u64) {
    resource.generation = generation;
}
pub fn bump_resource_generation(resource: &mut Resource) -> u64 {
    resource.generation = resource.generation.wrapping_add(1);
    resource.generation
}
pub fn bump_resource_level_generation(
    resource: &mut Resource,
    level: u32,
) -> ResourceGenerationResult {
    match resource.levels.get_mut(&level) {
        Some(value) => {
            value.generation = value.generation.wrapping_add(1);
            ResourceGenerationResult {
                status: status(StatusCode::Ok, ""),
                generation: value.generation,
            }
        }
        None => ResourceGenerationResult {
            status: status(
                StatusCode::InvalidArgument,
                "BumpResourceLevelGeneration: level out of range",
            ),
            generation: 0,
        },
    }
}
pub fn resource_generation(resource: &Resource) -> u64 {
    resource.generation
}
pub fn set_resource_level(
    resource: &mut Resource,
    level: u32,
    width: u32,
    height: u32,
    pitch: u32,
    byte_len: u32,
    generation: u64,
) -> Status {
    if width == 0 || height == 0 || pitch == 0 || byte_len == 0 {
        return status(
            StatusCode::InvalidArgument,
            "SetResourceLevel: invalid layout",
        );
    }
    resource.levels.insert(
        level,
        ResourceLevelData {
            width,
            height,
            pitch,
            byte_len,
            generation,
        },
    );
    resource.info.level_count = resource.info.level_count.max(level.saturating_add(1));
    status(StatusCode::Ok, "")
}
pub fn resource_level(resource: &Resource, level: u32) -> ResourceLevelResult {
    match resource.levels.get(&level) {
        Some(value) => ResourceLevelResult {
            status: status(StatusCode::Ok, ""),
            level: ResourceLevel {
                width: value.width,
                height: value.height,
                pitch: value.pitch,
                byte_len: value.byte_len,
                generation: value.generation,
            },
        },
        None => ResourceLevelResult {
            status: status(
                StatusCode::InvalidArgument,
                "GetResourceLevel: level out of range",
            ),
            level: ResourceLevel {
                width: 0,
                height: 0,
                pitch: 0,
                byte_len: 0,
                generation: 0,
            },
        },
    }
}

pub fn configure_texture_resource(
    resource: &mut Resource,
    width: u32,
    height: u32,
    levels: u32,
    format: u32,
) -> ResourceConfigurationResult {
    let first = match level_layout(width, height, levels, 0, format) {
        Ok(value) => value,
        Err(error) => {
            return ResourceConfigurationResult {
                status: status_from_error(error),
                level_count: 0,
                levels: Vec::new(),
            };
        }
    };
    let mut configured = Vec::new();
    if configured.try_reserve_exact(first.levels as usize).is_err() {
        return ResourceConfigurationResult {
            status: status(
                StatusCode::OutOfMemory,
                "ConfigureTexture: level metadata allocation failed",
            ),
            level_count: 0,
            levels: Vec::new(),
        };
    }
    for index in 0..first.levels {
        let layout = match level_layout(width, height, first.levels, index, format) {
            Ok(value) => value,
            Err(error) => {
                return ResourceConfigurationResult {
                    status: status_from_error(error),
                    level_count: 0,
                    levels: Vec::new(),
                };
            }
        };
        let value = ResourceLevel {
            width: layout.width,
            height: layout.height,
            pitch: layout.pitch,
            byte_len: layout.size,
            generation: 0,
        };
        configured.push(value);
    }
    resource.levels.clear();
    for (index, value) in configured.iter().enumerate() {
        resource.levels.insert(
            index as u32,
            ResourceLevelData {
                width: value.width,
                height: value.height,
                pitch: value.pitch,
                byte_len: value.byte_len,
                generation: value.generation,
            },
        );
    }
    resource.info.level_count = first.levels;
    ResourceConfigurationResult {
        status: status(StatusCode::Ok, ""),
        level_count: first.levels,
        levels: configured,
    }
}

pub fn create_shader(
    device: &mut Device,
    pixel: bool,
    declaration: &[u32],
    function: &[u32],
) -> ShaderHandleResult {
    if device.next_shader == u32::MAX {
        return ShaderHandleResult {
            status: status(
                StatusCode::InvalidArgument,
                "CreateShader: handle range exhausted",
            ),
            handle: 0,
        };
    }
    if declaration.len() > 4096 || function.len() > 4096 {
        return ShaderHandleResult {
            status: status(
                StatusCode::InvalidArgument,
                "CreateShader: token count exceeds 4096",
            ),
            handle: 0,
        };
    }
    if pixel && function.is_empty() {
        return ShaderHandleResult {
            status: status(
                StatusCode::InvalidArgument,
                "CreatePixelShader: empty function",
            ),
            handle: 0,
        };
    }
    let parsed_declaration = if pixel {
        None
    } else {
        if declaration.is_empty() {
            return ShaderHandleResult {
                status: status(
                    StatusCode::InvalidArgument,
                    "CreateVertexShader: empty declaration",
                ),
                handle: 0,
            };
        }
        match Declaration::parse(declaration) {
            Ok(value) => Some(value),
            Err(error) => {
                return ShaderHandleResult {
                    status: status_from_error(error),
                    handle: 0,
                };
            }
        }
    };
    let parsed_program = if function.is_empty() {
        if let Err(error) = parsed_declaration.as_ref().unwrap().fixed_fvf() {
            return ShaderHandleResult {
                status: status_from_error(error),
                handle: 0,
            };
        }
        Program {
            pixel: false,
            declaration_only: true,
            body: String::new(),
            samplers: [false; 4],
            cube_reflection: false,
        }
    } else {
        match Program::parse(function, pixel) {
            Ok(value) => value,
            Err(error) => {
                return ShaderHandleResult {
                    status: status_from_error(error),
                    handle: 0,
                };
            }
        }
    };
    let handle = device.next_shader;
    if let Some(renderer) = device.gpu.as_mut() {
        renderer
            .shaders
            .insert(handle, (parsed_declaration.clone(), parsed_program.clone()));
    }
    device.shaders.insert(
        handle,
        ShaderRecord {
            pixel,
            declaration: declaration.to_vec(),
            function: function.to_vec(),
            parsed_declaration,
            parsed_program,
        },
    );
    device.next_shader += 1;
    ShaderHandleResult {
        status: status(StatusCode::Ok, ""),
        handle,
    }
}

pub fn bind_shader(device: &mut Device, pixel: bool, handle: u32) -> Status {
    if pixel {
        if handle != 0
            && !device
                .shaders
                .get(&handle)
                .is_some_and(|shader| shader.pixel)
        {
            return status(
                StatusCode::InvalidArgument,
                "SetPixelShader: invalid shader handle",
            );
        }
        device.pixel_shader = handle;
        if let Some(renderer) = device.gpu.as_mut() {
            renderer.pixel_shader = handle;
        }
    } else if handle == 0 {
        device.vertex_shader = 0;
        device.vertex_fvf = 0;
        if let Some(renderer) = device.gpu.as_mut() {
            renderer.vertex_shader = 0;
        }
    } else if device
        .shaders
        .get(&handle)
        .is_some_and(|shader| !shader.pixel)
    {
        device.vertex_shader = handle;
        device.vertex_fvf = 0;
        if let Some(renderer) = device.gpu.as_mut() {
            renderer.vertex_shader = handle;
        }
    } else {
        if handle > 0xf000_0000 {
            return status(
                StatusCode::InvalidArgument,
                "SetVertexShader: invalid shader handle or stage",
            );
        }
        if let Err(error) = crate::d3d8::fixed_function::FvfLayout::decode(handle) {
            return status_from_error(error);
        }
        device.vertex_shader = 0;
        device.vertex_fvf = handle;
        if let Some(renderer) = device.gpu.as_mut() {
            renderer.vertex_shader = 0;
        }
    }
    status(StatusCode::Ok, "")
}

pub fn delete_shader(device: &mut Device, pixel: bool, handle: u32) -> Status {
    if !device
        .shaders
        .get(&handle)
        .is_some_and(|shader| shader.pixel == pixel)
    {
        return status(
            StatusCode::InvalidArgument,
            "DeleteShader: invalid shader handle",
        );
    }
    if let Some(renderer) = device.gpu.as_mut() {
        renderer.retire_shader(handle);
        if pixel && renderer.pixel_shader == handle {
            renderer.pixel_shader = 0;
        }
        if !pixel && renderer.vertex_shader == handle {
            renderer.vertex_shader = 0;
        }
        renderer.shaders.remove(&handle);
    }
    if pixel && device.pixel_shader == handle {
        device.pixel_shader = 0;
    }
    if !pixel && device.vertex_shader == handle {
        device.vertex_shader = 0;
    }
    device.shaders.remove(&handle);
    status(StatusCode::Ok, "")
}

pub fn selected_shader(device: &Device, pixel: bool) -> u32 {
    if pixel {
        device.pixel_shader
    } else if device.vertex_shader != 0 {
        device.vertex_shader
    } else {
        device.vertex_fvf
    }
}

pub fn shader_stream_count(device: &Device) -> u32 {
    device
        .shaders
        .get(&device.vertex_shader)
        .and_then(|s| s.parsed_declaration.as_ref())
        .map_or(1, |d| d.stream_count)
}

pub fn draw_vertex_streams(
    device: &mut Device,
    topology: u32,
    streams: &[ffi::VertexStream],
    start_vertex: u32,
    primitive_count: u32,
    indexed: bool,
    indices: &[u8],
    index_format: u32,
    base_vertex: u32,
    min_index: u32,
    num_vertices: u32,
    start_index: u32,
) -> Status {
    use crate::d3d9::draw::{DrawCommand, expanded_indices};
    use crate::fast_map::FastMap;
    let Some(declaration) = device
        .shaders
        .get(&device.vertex_shader)
        .and_then(|s| s.parsed_declaration.as_ref())
    else {
        return status(
            StatusCode::InvalidArgument,
            "Draw: multi-stream shader has no declaration",
        );
    };
    let command = DrawCommand {
        primitive: topology,
        primitive_count,
        start_vertex,
        base_vertex: base_vertex as i32,
        indexed,
        index_format,
        index_bytes: if indexed {
            let size = if index_format == 101 { 2usize } else { 4 };
            let Some(at) = (start_index as usize).checked_mul(size) else {
                return status(StatusCode::InvalidArgument, "Draw: index offset overflow");
            };
            let Some(bytes) = indices.get(at..) else {
                return status(
                    StatusCode::InvalidArgument,
                    "Draw: index offset outside buffer",
                );
            };
            bytes.to_vec().into()
        } else {
            Vec::new().into()
        },
        min_vertex: min_index,
        num_vertices,
        streams: Default::default(),
    };
    let (order, draw_indices, _, gpu_topology) =
        match expanded_indices(&command, &mut Vec::new(), &mut FastMap::default()) {
            Ok(value) => value,
            Err(error) => return status_from_error(error),
        };
    let order = match draw_indices {
        Some(indices) => indices
            .into_iter()
            .map(|index| order[index as usize])
            .collect(),
        None => order,
    };
    let vertices = match declaration.pack_vertices(streams, &order) {
        Ok(value) => value,
        Err(error) => return status_from_error(error),
    };
    let stride = declaration.layout.stride as u32;
    let topology = match gpu_topology {
        wgpu::PrimitiveTopology::PointList => 1,
        wgpu::PrimitiveTopology::LineList => 2,
        wgpu::PrimitiveTopology::LineStrip => 3,
        wgpu::PrimitiveTopology::TriangleList => 4,
        wgpu::PrimitiveTopology::TriangleStrip => 5,
    };
    draw_primitive(
        device,
        topology,
        device.vertex_shader,
        &vertices,
        stride,
        0,
        primitive_count,
    )
}

pub fn set_shader_constants(
    device: &mut Device,
    pixel: bool,
    register_index: u32,
    values: &[u32],
) -> Status {
    let constants = if pixel {
        &mut device.pixel_constants
    } else {
        &mut device.vertex_constants
    };
    if values.len() % 4 != 0
        || (!values.is_empty()
            && (register_index as usize > constants.len() / 4
                || values.len() / 4 > constants.len() / 4 - register_index as usize))
    {
        return status(
            StatusCode::InvalidArgument,
            "SetShaderConstant: invalid register range",
        );
    }
    if values.is_empty() {
        return status(StatusCode::Ok, "");
    }
    let start = register_index as usize * 4;
    constants[start..start + values.len()].copy_from_slice(values);
    if let Some(renderer) = device.gpu.as_mut() {
        let bank = if pixel {
            &mut renderer.shader_uniform.pc[..]
        } else {
            &mut renderer.shader_uniform.vc[..]
        };
        for (index, chunk) in values.chunks_exact(4).enumerate() {
            bank[register_index as usize + index] =
                std::array::from_fn(|component| f32::from_bits(chunk[component]));
        }
    }
    status(StatusCode::Ok, "")
}

pub fn get_shader_constants(
    device: &Device,
    pixel: bool,
    register_index: u32,
    count: u32,
) -> ConstantRead {
    let constants = if pixel {
        &device.pixel_constants
    } else {
        &device.vertex_constants
    };
    if count == 0 {
        return ConstantRead {
            status: status(StatusCode::Ok, ""),
            values: Vec::new(),
        };
    }
    let start = register_index as usize * 4;
    let len = count as usize * 4;
    if start > constants.len() || len > constants.len() - start {
        return ConstantRead {
            status: status(
                StatusCode::InvalidArgument,
                "GetShaderConstant: invalid register range",
            ),
            values: Vec::new(),
        };
    }
    ConstantRead {
        status: status(StatusCode::Ok, ""),
        values: constants[start..start + len].to_vec(),
    }
}

pub fn shader_tokens(device: &Device, pixel: bool, handle: u32, declaration: bool) -> ValuesResult {
    let Some(shader) = device
        .shaders
        .get(&handle)
        .filter(|shader| shader.pixel == pixel)
    else {
        return ValuesResult {
            status: status(
                StatusCode::InvalidArgument,
                "GetShader: invalid handle or stage",
            ),
            values: Vec::new(),
        };
    };
    if declaration {
        if pixel {
            return ValuesResult {
                status: status(
                    StatusCode::InvalidArgument,
                    "GetShaderDeclaration: pixel shader has no declaration",
                ),
                values: Vec::new(),
            };
        }
        ValuesResult {
            status: status(StatusCode::Ok, ""),
            values: shader.declaration.clone(),
        }
    } else {
        ValuesResult {
            status: status(StatusCode::Ok, ""),
            values: shader.function.clone(),
        }
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self { storage: None }
    }
}
pub fn new_storage() -> Box<Storage> {
    Box::default()
}
pub fn allocate_storage(storage: &mut Storage, size: u32) -> Status {
    match crate::d3d8::resource::CpuStorage::new(size) {
        Ok(value) => {
            storage.storage = Some(value);
            status(StatusCode::Ok, "")
        }
        Err(error) => status_from_error(error),
    }
}
pub fn storage_bytes(storage: &Storage) -> &[u8] {
    storage.storage.as_ref().map_or(&[], |value| &value.bytes)
}
pub fn storage_bytes_mut(storage: &mut Storage) -> &mut [u8] {
    storage
        .storage
        .as_mut()
        .map_or(&mut [], |value| &mut value.bytes)
}
pub fn copy_storage(destination: &mut Storage, source: &Storage) -> Status {
    let (Some(destination), Some(source)) = (destination.storage.as_mut(), source.storage.as_ref())
    else {
        return status(
            StatusCode::InvalidArgument,
            "UpdateTexture: storage is not allocated",
        );
    };
    if destination.bytes.len() != source.bytes.len() {
        return status(
            StatusCode::InvalidArgument,
            "UpdateTexture: storage sizes differ",
        );
    }
    destination.bytes.copy_from_slice(&source.bytes);
    status(StatusCode::Ok, "")
}
pub fn format_bytes(format: u32) -> u32 {
    cpu_format_bytes(format)
}
pub fn block_bytes(format: u32) -> u32 {
    crate::d3d8::format::block_bytes(format)
}
pub fn texture_level_layout(
    width: u32,
    height: u32,
    levels: u32,
    level: u32,
    format: u32,
) -> LevelLayoutResult {
    match level_layout(width, height, levels, level, format) {
        Ok(layout) => LevelLayoutResult {
            status: status(StatusCode::Ok, ""),
            layout: layout.into(),
        },
        Err(error) => LevelLayoutResult {
            status: status_from_error(error),
            layout: LevelLayout::default(),
        },
    }
}

impl Default for LevelLayout {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            pitch: 0,
            size: 0,
            levels: 0,
        }
    }
}
impl From<D3d8LevelLayout> for LevelLayout {
    fn from(layout: D3d8LevelLayout) -> Self {
        Self {
            width: layout.width,
            height: layout.height,
            pitch: layout.pitch,
            size: layout.size,
            levels: layout.levels,
        }
    }
}

pub fn new_device7_state() -> Box<Device7State> {
    Box::default()
}

fn value_result(value: u32) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value,
    }
}
fn invalid_value(message: &str) -> ValueResult {
    ValueResult {
        status: status(StatusCode::InvalidArgument, message),
        value: 0,
    }
}

pub fn d3d7_get_render_state(state: &Device7State, index: u32) -> ValueResult {
    state
        .live
        .render_state
        .get(index as usize)
        .copied()
        .map(value_result)
        .unwrap_or_else(|| invalid_value("GetRenderState: index out of range"))
}
pub fn d3d7_set_render_state(state: &mut Device7State, index: u32, value: u32) -> Status {
    let Some(index) = usize::try_from(index).ok().filter(|i| *i < 256) else {
        return status(
            StatusCode::InvalidArgument,
            "SetRenderState: index out of range",
        );
    };
    if crate::bridge_translate::translate_render_state(index as u32, value).kind
        == D3d7TranslationKind::Invalid
    {
        return status(
            StatusCode::Unsupported,
            format!("SetRenderState: no D3D8 translation for state {index} value {value}"),
        );
    }
    let target = state_write_target(state);
    target.render_state[index] = value;
    target.render_state_set[index] = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_render_state_was_set(state: &Device7State, index: u32) -> bool {
    state
        .live
        .render_state_set
        .get(index as usize)
        .copied()
        .unwrap_or(false)
}

pub fn d3d7_get_texture_stage_state(state: &Device7State, stage: u32, index: u32) -> ValueResult {
    match (
        state.live.texture_stage_state.get(stage as usize),
        state.live.texture_stage_state_set.get(stage as usize),
    ) {
        (Some(values), _) if (index as usize) < values.len() => {
            value_result(values[index as usize])
        }
        _ => invalid_value("GetTextureStageState: stage or index out of range"),
    }
}
pub fn d3d7_set_texture_stage_state(
    state: &mut Device7State,
    stage: u32,
    index: u32,
    value: u32,
) -> Status {
    let (Some(stage), Some(index)) = (
        usize::try_from(stage).ok().filter(|i| *i < 8),
        usize::try_from(index).ok().filter(|i| *i < 256),
    ) else {
        return status(
            StatusCode::InvalidArgument,
            "SetTextureStageState: stage or index out of range",
        );
    };
    if crate::bridge_translate::translate_texture_stage_state(index as u32).count < 0 {
        return status(
            StatusCode::Unsupported,
            format!("SetTextureStageState: no D3D8 translation for state {index}"),
        );
    }
    let target = state_write_target(state);
    target.texture_stage_state[stage][index] = value;
    target.texture_stage_state_set[stage][index] = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_texture_stage_state_was_set(state: &Device7State, stage: u32, index: u32) -> bool {
    state
        .live
        .texture_stage_state_set
        .get(stage as usize)
        .and_then(|row| row.get(index as usize))
        .copied()
        .unwrap_or(false)
}

pub fn d3d7_get_transform(state: &Device7State, index: u32) -> FloatValuesResult {
    match state.live.transforms.get(index as usize) {
        Some(values) => FloatValuesResult {
            status: status(StatusCode::Ok, ""),
            values: values.to_vec(),
        },
        None => FloatValuesResult {
            status: status(
                StatusCode::InvalidArgument,
                "GetTransform: index out of range",
            ),
            values: Vec::new(),
        },
    }
}
pub fn d3d7_set_transform(state: &mut Device7State, index: u32, values: &[f32]) -> Status {
    let Some(index) = usize::try_from(index).ok().filter(|i| *i < 256) else {
        return status(
            StatusCode::InvalidArgument,
            "SetTransform: index out of range",
        );
    };
    if !crate::bridge_translate::translate_transform(index as u32).valid {
        return status(
            StatusCode::Unsupported,
            format!("SetTransform: no D3D8 translation for state {index}"),
        );
    }
    let Ok(matrix) = <&[f32; 16]>::try_from(values) else {
        return status(
            StatusCode::InvalidArgument,
            "SetTransform: expected 16 floats",
        );
    };
    let target = state_write_target(state);
    target.transforms[index] = *matrix;
    target.transform_set[index] = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_transform_was_set(state: &Device7State, index: u32) -> bool {
    state
        .live
        .transform_set
        .get(index as usize)
        .copied()
        .unwrap_or(false)
}

pub fn d3d7_get_viewport(state: &Device7State) -> ViewportResult {
    ViewportResult {
        status: status(StatusCode::Ok, ""),
        viewport: state.live.viewport,
    }
}
pub fn d3d7_set_viewport(state: &mut Device7State, viewport: &Viewport) -> Status {
    let target = state_write_target(state);
    target.viewport = *viewport;
    target.viewport_set = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_viewport_was_set(state: &Device7State) -> bool {
    state.live.viewport_set
}

pub fn d3d7_get_material(state: &Device7State) -> ValuesResult {
    ValuesResult {
        status: status(StatusCode::Ok, ""),
        values: state.live.material.to_vec(),
    }
}
pub fn d3d7_set_material(state: &mut Device7State, values: &[u32]) -> Status {
    let Ok(values) = <&[u32; 17]>::try_from(values) else {
        return status(
            StatusCode::InvalidArgument,
            "SetMaterial: expected 17 words",
        );
    };
    let target = state_write_target(state);
    target.material = *values;
    target.material_set = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_material_was_set(state: &Device7State) -> bool {
    state.live.material_set
}

pub fn d3d7_get_light(state: &Device7State, index: u32) -> ValuesResult {
    match state.live.lights.get(index as usize) {
        Some(values) => ValuesResult {
            status: status(StatusCode::Ok, ""),
            values: values.to_vec(),
        },
        None => ValuesResult {
            status: status(StatusCode::InvalidArgument, "GetLight: index out of range"),
            values: Vec::new(),
        },
    }
}
pub fn d3d7_set_light(state: &mut Device7State, index: u32, values: &[u32]) -> Status {
    let Some(index) = usize::try_from(index).ok().filter(|i| *i < 8) else {
        return status(StatusCode::InvalidArgument, "SetLight: index out of range");
    };
    let Ok(values) = <&[u32; 26]>::try_from(values) else {
        return status(StatusCode::InvalidArgument, "SetLight: expected 26 words");
    };
    let target = state_write_target(state);
    target.lights[index] = *values;
    target.light_set[index] = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_get_light_enable(state: &Device7State, index: u32) -> ValueResult {
    state
        .live
        .light_enable
        .get(index as usize)
        .copied()
        .map(value_result)
        .unwrap_or_else(|| invalid_value("GetLightEnable: index out of range"))
}
pub fn d3d7_set_light_enable(state: &mut Device7State, index: u32, value: u32) -> Status {
    let Some(index) = usize::try_from(index).ok().filter(|i| *i < 8) else {
        return status(
            StatusCode::InvalidArgument,
            "SetLightEnable: index out of range",
        );
    };
    let target = state_write_target(state);
    target.light_enable[index] = u32::from(value != 0);
    target.light_enable_set[index] = true;
    status(StatusCode::Ok, "")
}
pub fn d3d7_get_texture(state: &Device7State, stage: u32) -> ValueResult {
    state
        .live
        .textures
        .get(stage as usize)
        .copied()
        .map(value_result)
        .unwrap_or_else(|| invalid_value("GetTexture: stage out of range"))
}
pub fn d3d7_set_texture(state: &mut Device7State, stage: u32, object_id: u32) -> TextureSetResult {
    let Some(stage) = usize::try_from(stage).ok().filter(|i| *i < 8) else {
        return TextureSetResult {
            status: status(
                StatusCode::InvalidArgument,
                "SetTexture: stage out of range",
            ),
            old_id: 0,
            new_id: object_id,
            recording: state.recording.is_some(),
        };
    };
    let recording = state.recording.is_some();
    let old_id = if recording {
        state.recording.as_ref().unwrap().texture_set[stage]
            .then_some(state.recording.as_ref().unwrap().textures[stage])
            .unwrap_or(0)
    } else {
        state.live.textures[stage]
    };
    let target = state_write_target(state);
    target.textures[stage] = object_id;
    target.texture_set[stage] = true;
    TextureSetResult {
        status: status(StatusCode::Ok, ""),
        old_id,
        new_id: object_id,
        recording,
    }
}
pub fn d3d7_get_render_target(state: &Device7State) -> u32 {
    state.live.render_target
}
pub fn d3d7_set_render_target(state: &mut Device7State, object_id: u32) {
    state.live.render_target = object_id;
}
pub fn d3d7_recording(state: &Device7State) -> bool {
    state.recording.is_some()
}
pub fn d3d7_in_scene(state: &Device7State) -> bool {
    state.live.in_scene
}
pub fn d3d7_set_in_scene(state: &mut Device7State, value: bool) {
    state.live.in_scene = value;
}
pub fn d3d7_target_dirty(state: &Device7State) -> bool {
    state.live.target_dirty
}
pub fn d3d7_set_target_dirty(state: &mut Device7State, value: bool) {
    state.live.target_dirty = value;
}

pub fn d3d7_begin_state_block(state: &mut Device7State) -> Status {
    if state.recording.is_some() {
        return state_block_status(
            StatusCode::InvalidArgument,
            "D3DERR_INBEGINSTATEBLOCK",
            StateBlockError::InBeginStateBlock,
        );
    }
    state.recording = Some(Device7Data::default());
    status(StatusCode::Ok, "")
}
pub fn d3d7_end_state_block(state: &mut Device7State) -> HandleResult {
    if state.recording.is_none() {
        return HandleResult {
            status: state_block_status(
                StatusCode::InvalidArgument,
                "D3DERR_NOTINBEGINSTATEBLOCK",
                StateBlockError::NotInBeginStateBlock,
            ),
            handle: 0,
        };
    }
    if state.next_state_block == u32::MAX {
        return HandleResult {
            status: state_block_status(
                StatusCode::OutOfMemory,
                "EndStateBlock: handle range exhausted",
                StateBlockError::OutOfMemory,
            ),
            handle: 0,
        };
    }
    let recorded = state.recording.take().unwrap();
    let handle = state.next_state_block;
    state.next_state_block += 1;
    state.state_blocks.insert(handle, recorded);
    HandleResult {
        status: status(StatusCode::Ok, ""),
        handle,
    }
}
pub fn d3d7_apply_state_block(state: &mut Device7State, handle: u32) -> ApplyStateBlockResult {
    if state.recording.is_some() {
        return ApplyStateBlockResult {
            status: state_block_status(
                StatusCode::InvalidArgument,
                "D3DERR_INBEGINSTATEBLOCK",
                StateBlockError::InBeginStateBlock,
            ),
            bindings: TextureBindings {
                old_values: Vec::new(),
                new_values: Vec::new(),
            },
        };
    }
    let Some(patch) = state.state_blocks.get(&handle).cloned() else {
        return ApplyStateBlockResult {
            status: state_block_status(
                StatusCode::InvalidArgument,
                "D3DERR_INVALIDSTATEBLOCK",
                StateBlockError::InvalidStateBlock,
            ),
            bindings: TextureBindings {
                old_values: Vec::new(),
                new_values: Vec::new(),
            },
        };
    };
    let old_values = state.live.textures.to_vec();
    for i in 0..256 {
        if patch.render_state_set[i] {
            state.live.render_state[i] = patch.render_state[i];
            state.live.render_state_set[i] = true;
        }
        if patch.transform_set[i] {
            state.live.transforms[i] = patch.transforms[i];
            state.live.transform_set[i] = true;
        }
        for stage in 0..8 {
            if patch.texture_stage_state_set[stage][i] {
                state.live.texture_stage_state[stage][i] = patch.texture_stage_state[stage][i];
                state.live.texture_stage_state_set[stage][i] = true;
            }
        }
    }
    if patch.viewport_set {
        state.live.viewport = patch.viewport;
        state.live.viewport_set = true;
    }
    if patch.material_set {
        state.live.material = patch.material;
        state.live.material_set = true;
    }
    for i in 0..8 {
        if patch.light_set[i] {
            state.live.lights[i] = patch.lights[i];
            state.live.light_set[i] = true;
        }
        if patch.light_enable_set[i] {
            state.live.light_enable[i] = patch.light_enable[i];
            state.live.light_enable_set[i] = true;
        }
        if patch.texture_set[i] {
            state.live.textures[i] = patch.textures[i];
            state.live.texture_set[i] = true;
        }
    }
    ApplyStateBlockResult {
        status: status(StatusCode::Ok, ""),
        bindings: TextureBindings {
            old_values,
            new_values: state.live.textures.to_vec(),
        },
    }
}
pub fn d3d7_delete_state_block(state: &mut Device7State, handle: u32) -> ValuesResult {
    let Some(block) = state.state_blocks.remove(&handle) else {
        return ValuesResult {
            status: state_block_status(
                StatusCode::InvalidArgument,
                "D3DERR_INVALIDSTATEBLOCK",
                StateBlockError::InvalidStateBlock,
            ),
            values: Vec::new(),
        };
    };
    let values = (0..8)
        .filter(|i| block.texture_set[*i])
        .map(|i| block.textures[i])
        .collect();
    ValuesResult {
        status: status(StatusCode::Ok, ""),
        values,
    }
}

pub fn d3d7_release_refs(state: &mut Device7State) -> ValuesResult {
    let mut values = Vec::new();
    values.extend(state.live.textures.iter().copied());
    if let Some(recording) = state.recording.take() {
        values.extend(
            (0..8)
                .filter(|i| recording.texture_set[*i])
                .map(|i| recording.textures[i]),
        );
    }
    for block in state.state_blocks.values() {
        values.extend(
            (0..8)
                .filter(|i| block.texture_set[*i])
                .map(|i| block.textures[i]),
        );
    }
    state.state_blocks.clear();
    state.live.textures.fill(0);
    state.live.texture_set.fill(false);
    ValuesResult {
        status: status(StatusCode::Ok, ""),
        values,
    }
}

pub fn d3d7_state_block_render_state_was_set(
    state: &Device7State,
    handle: u32,
    index: u32,
) -> bool {
    state
        .state_blocks
        .get(&handle)
        .and_then(|b| b.render_state_set.get(index as usize))
        .copied()
        .unwrap_or(false)
}
pub fn d3d7_state_block_texture_stage_state_was_set(
    state: &Device7State,
    handle: u32,
    stage: u32,
    index: u32,
) -> bool {
    state
        .state_blocks
        .get(&handle)
        .and_then(|b| b.texture_stage_state_set.get(stage as usize))
        .and_then(|v| v.get(index as usize))
        .copied()
        .unwrap_or(false)
}
pub fn d3d7_state_block_transform_was_set(state: &Device7State, handle: u32, index: u32) -> bool {
    state
        .state_blocks
        .get(&handle)
        .and_then(|b| b.transform_set.get(index as usize))
        .copied()
        .unwrap_or(false)
}
pub fn d3d7_state_block_viewport_was_set(state: &Device7State, handle: u32) -> bool {
    state
        .state_blocks
        .get(&handle)
        .is_some_and(|b| b.viewport_set)
}
pub fn d3d7_state_block_material_was_set(state: &Device7State, handle: u32) -> bool {
    state
        .state_blocks
        .get(&handle)
        .is_some_and(|b| b.material_set)
}
pub fn d3d7_state_block_light_was_set(state: &Device7State, handle: u32, index: u32) -> bool {
    state
        .state_blocks
        .get(&handle)
        .and_then(|b| b.light_set.get(index as usize))
        .copied()
        .unwrap_or(false)
}
pub fn d3d7_state_block_light_enable_was_set(
    state: &Device7State,
    handle: u32,
    index: u32,
) -> bool {
    state
        .state_blocks
        .get(&handle)
        .and_then(|b| b.light_enable_set.get(index as usize))
        .copied()
        .unwrap_or(false)
}
pub fn d3d7_state_block_texture_was_set(state: &Device7State, handle: u32, stage: u32) -> bool {
    state
        .state_blocks
        .get(&handle)
        .and_then(|b| b.texture_set.get(stage as usize))
        .copied()
        .unwrap_or(false)
}
