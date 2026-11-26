# Issue #153: план D3D12 diagnostics-only эксперимента

**Дата:** 2026-11-26  
**Репозиторий:** https://github.com/vanya890/VIctory  
**Исследуемая ветка:** `dev`  
**Базовый SHA (проверен созданием временной ветки от dev и compare):** `b12aaf73f0ce1f916ffdcbe6c741b19b1cb50bfe`  
**Предыдущий отчёт:** [issue-153-d3d12-investigation.md](issue-153-d3d12-investigation.md), commit `b12aaf73f0ce1f916ffdcbe6c741b19b1cb50bfe`  
**Исследуемый файл:** `crates/victory-gpu/src/host.rs`, blob SHA `abae5d777dc6325673f2c26de518f9657199b4ec`  
**Cargo bindings:** `windows = 0.62.2`, `windows-sys = 0.61.2` (workspace dependencies)  
**Результат:** подготовлен test-only диагностический эксперимент. Исходники и тесты не изменялись.

> Этот документ — план эксперимента и готовый локальный Rust snippet, а не утверждение, что эксперимент уже исполнялся. Никаких фактических capability values, adapter details или debug messages пока нет.

---

## 1. Резюме и границы

### Подтверждено исходным кодом и CI-отчётом

1. В тесте `host::present_plane_tests::present_conversion_requires_typed_view_and_shader_resource_access` первый ресурс создаётся как `DXGI_FORMAT_R16G16B16A16_TYPELESS` с `D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET` и initial state `D3D12_RESOURCE_STATE_RENDER_TARGET`.
2. В предыдущем CI запуске `CreateCommittedResource` вернул `HRESULT(0x80070057)`. Падение происходило на `unwrap()` внутри closure `make`.
3. До создания RTV/SRV, compositor, shader pipeline, command submission и barriers выполнение не доходило.
4. В `host.rs::create()` Debug Layer включается до `D3D12CreateDevice`, если одновременно собрана feature `diagnostics` и включён `VICTORY_D3D12_DEBUG`.
5. Путь WARP выбирается только при `diagnostics` + `VICTORY_D3D12_WARP`; иначе вызывается `D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, ...)`.
6. После создания устройства код пытается получить `ID3D12InfoQueue`, но отдельный тест не получает полный snapshot сообщений непосредственно вокруг каждой попытки `CreateCommittedResource`.
7. В текущем тесте `make()` вызывает `unwrap()`, поэтому первая ошибка прекращает матрицу и не даёт результатов следующих вариантов.

### Не доказано / гипотезы

- Не установлено, что типизированный член семейства действительно не поддерживает RT на проблемном адаптере.
- Не установлено, что проблема вызвана конкретным полем `D3D12_RESOURCE_DESC`.
- Не установлено, что это баг драйвера, hosted adapter или D3D12 runtime.
- Нельзя трактовать отсутствие RT-бита на самом typeless формате как самостоятельное доказательство запрета typeless resource + RT flag. Нужно сопоставлять capability typed members и текст validation layer.
- Нельзя утверждать, что Debug Layer изменил или исправил поведение: он должен диагностировать некорректное использование, а не быть production workaround.

**Никаких production изменений не предлагается.** Тест не отключать, не игнорировать HRESULT, не менять `main`, не менять `dev` и не подменять проверяемую семантику typed-view conversion.

---

## 2. Состояние dev и архитектура

### Текущий baseline

Временная ветка `agent/issue-153-d3d12-diagnostics-plan` была создана непосредственно от `dev`. GitHub compare `dev...agent/issue-153-d3d12-diagnostics-plan` вернул `identical`, а `base_commit.sha` — `b12aaf73f0ce1f916ffdcbe6c741b19b1cb50bfe`. Следовательно, на момент создания ветки текущий HEAD `dev` точно совпадал с SHA предыдущего отчёта.

Blob SHA `host.rs` подтверждён повторным чтением из `dev`: `abae5d777dc6325673f2c26de518f9657199b4ec`.

### Device initialization: `crates/victory-gpu/src/host.rs::create()`

Фактический порядок:

1. Вычисляется `debug = cfg!(feature = "diagnostics") && switch::on("VICTORY_D3D12_DEBUG")`.
2. Если debug включён, вызывается `D3D12GetDebugInterface`, затем `ID3D12Debug::EnableDebugLayer()`.
3. При отдельном `VICTORY_D3D12_GBV` на debug interface включается GPU-Based Validation. Для этого эксперимента GBV не требуется и включать его не следует.
4. Отдельно может быть включён DRED.
5. Если `diagnostics` + `VICTORY_D3D12_WARP`, выбирается `warp_adapter()`, затем `D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, ...)`.
6. Иначе используется `D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, ...)`.
7. После успешного создания устройства проверяется OPTIONS2, затем выполняется cast в `ID3D12InfoQueue`.
8. Далее создаются очередь, fence и descriptor heaps.

Это уже правильная последовательность для включения D3D12 Debug Layer: **получить debug interface → EnableDebugLayer → D3D12CreateDevice**. Microsoft прямо указывает, что включение после создания устройства приведёт к удалению device. Поэтому не следует добавлять поздний вызов `EnableDebugLayer`.

### Adapter identity

D3D12 device предоставляет `GetAdapterLuid()`. Для получения именно используемого адаптера следует:
- вызвать `device.GetAdapterLuid()`;
- получить `IDXGIFactory4` через `CreateDXGIFactory2`;
- вызвать `EnumAdapterByLuid(luid)`;
- cast в `IDXGIAdapter1`;
- вызвать `GetDesc1()`.

Из `DXGI_ADAPTER_DESC1` доступны `Description`, `VendorId`, `DeviceId`, `Revision`, `Flags`, `AdapterLuid`. Это надёжнее, чем перечислять адаптеры и предполагать, какой из них выбрал D3D12.

Для driver version можно best-effort вызвать `IDXGIAdapter::CheckInterfaceSupport` с IID `IDXGIDevice`. Microsoft описывает это как legacy UMD-version query; на новых системах вызов может вернуть unsupported. При успехе вывести 64-bit значение и декодировать его как четыре 16-bit компонента. Ошибка запроса не должна прерывать эксперимент. В отчёте явно указывать «unavailable», если HRESULT неуспешен.

### Feature level

`device.GetFeatureLevel()` — фактический feature level созданного device. Это не результат запроса «максимального уровня адаптера»: D3D12CreateDevice в данном проекте запрашивает `D3D_FEATURE_LEVEL_12_0`. Логировать возвращённый уровень, а не выводить его из версии Windows или GPU.

### WARP / hardware

Логировать отдельно:
- `VICTORY_D3D12_WARP` был включён или нет;
- путь выбора adapter: explicit WARP или default adapter selection;
- `DXGI_ADAPTER_FLAG_SOFTWARE` из adapter description.

Не считать `SOFTWARE` flag универсальным доказательством именно WARP: это тип software adapter. В данном приложении факт использования WARP определяется выбранной веткой `VICTORY_D3D12_WARP` и `EnumWarpAdapter`.

---

## 3. Format support queries

Нужно выполнить пять независимых запросов `CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, ...)`:

| DXGI format | Зачем |
|---|---|
| `R16G16B16A16_TYPELESS` | Проверить результат драйвера для исходного resource format |
| `R16G16B16A16_UNORM` | Проверить typed UNORM RTV/SRV member |
| `R16G16B16A16_FLOAT` | Проверить typed FLOAT member |
| `B8G8R8X8_TYPELESS` | Контрольный typeless format, создаваемый соседним тестом |
| `B8G8R8X8_UNORM` | Typed control member |

Для каждого формата логировать:
- имя DXGI format;
- HRESULT `CheckFeatureSupport`;
- `Support1` как `0xXXXXXXXX`;
- `Support2` как `0xXXXXXXXX`;
- декодированные flags (только если query успешен).

### Важные Support1 flags

| Flag | Value | Значение |
|---|---:|---|
| `BUFFER` | `0x00000001` | buffer |
| `TEXTURE1D` | `0x00000010` | Texture1D |
| `TEXTURE2D` | `0x00000020` | Texture2D |
| `TEXTURE3D` | `0x00000040` | Texture3D |
| `TEXTURECUBE` | `0x00000080` | TextureCube |
| `SHADER_LOAD` | `0x00000100` | shader Load |
| `SHADER_SAMPLE` | `0x00000200` | shader sampling |
| `MIP` | `0x00001000` | mipmaps |
| `RENDER_TARGET` | `0x00004000` | render-target usage |
| `BLENDABLE` | `0x00008000` | blending |
| `DEPTH_STENCIL` | `0x00010000` | depth/stencil |
| `MULTISAMPLE_RESOLVE` | `0x00040000` | resolve |
| `DISPLAY` | `0x00080000` | display |
| `CAST_WITHIN_BIT_LAYOUT` | `0x00100000` | format cast within bit layout |
| `MULTISAMPLE_RENDERTARGET` | `0x00200000` | multisampled RT |
| `TYPED_UNORDERED_ACCESS_VIEW` | `0x02000000` | typed UAV |

### Support2 flags

| Flag | Value |
|---|---:|
| `UAV_ATOMIC_ADD` | `0x00000001` |
| `UAV_ATOMIC_BITWISE_OPS` | `0x00000002` |
| `UAV_ATOMIC_COMPARE_STORE_OR_COMPARE_EXCHANGE` | `0x00000004` |
| `UAV_ATOMIC_EXCHANGE` | `0x00000008` |
| `UAV_ATOMIC_SIGNED_MIN_OR_MAX` | `0x00000010` |
| `UAV_ATOMIC_UNSIGNED_MIN_OR_MAX` | `0x00000020` |
| `UAV_TYPED_LOAD` | `0x00000040` |
| `UAV_TYPED_STORE` | `0x00000080` |
| `OUTPUT_MERGER_LOGIC_OP` | `0x00000100` |
| `TILED` | `0x00000200` |
| `MULTIPLANE_OVERLAY` | `0x00004000` |

`Support2` описывает unordered-resource options; его отсутствие UAV bits само по себе не является причиной отказа обычного RT resource.

---

## 4. Минимальный Rust snippet (локальный test-only эксперимент)

Ниже — добавляемый локально код для временного эксперимента. Его следует поместить рядом с `present_plane_tests` в `host.rs` или в отдельный временный test-only модуль. Он не является diff для production. Запускать только на Windows.

Используются уже включённые в `crates/victory-gpu/Cargo.toml` windows features: Direct3D12, Direct3D, DXGI и DXGI Common. Для IID `IDXGIDevice` импортировать `IDXGIDevice` из `windows::Win32::Graphics::Dxgi`.

```rust
#[cfg(test)]
fn d3d12_issue153_diagnostics(device: &ID3D12Device, is_warp: bool) {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIAdapter1, IDXGIDevice};
    use windows::Win32::Graphics::Dxgi::Common::*;

    unsafe {
        // 1. Resolve the adapter actually used by this D3D12 device.
        let luid = device.GetAdapterLuid();
        let factory: IDXGIFactory4 =
            match CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ISSUE153 adapter factory HRESULT={:#010X}: {e}",
                        e.code().0 as u32);
                    return;
                }
            };
        let adapter = match factory.EnumAdapterByLuid(luid) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ISSUE153 EnumAdapterByLuid HRESULT={:#010X}: {e}",
                    e.code().0 as u32);
                return;
            }
        };
        let adapter1: IDXGIAdapter1 = match adapter.cast() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ISSUE153 IDXGIAdapter1 cast HRESULT={:#010X}: {e}",
                    e.code().0 as u32);
                return;
            }
        };
        match adapter1.GetDesc1() {
            Ok(desc) => {
                let name = String::from_utf16_lossy(
                    &desc.Description[..desc.Description.iter().position(|&c| c == 0)
                        .unwrap_or(desc.Description.len())]
                );
                eprintln!(
                    "ISSUE153 adapter name={name:?} vendor={:#06X} device={:#06X} revision={:#06X} flags={:#010X} luid={:08X}:{:08X} feature_level={:#06X} selection={}",
                    desc.VendorId, desc.DeviceId, desc.Revision, desc.Flags,
                    luid.HighPart as u32, luid.LowPart,
                    device.GetFeatureLevel().0,
                    if is_warp { "WARP(EnumWarpAdapter)" } else { "default D3D12CreateDevice(None)" },
                );
                eprintln!(
                    "ISSUE153 adapter software_flag={}",
                    (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0
                );
            }
            Err(e) => eprintln!("ISSUE153 GetDesc1 HRESULT={:#010X}: {e}",
                e.code().0 as u32),
        }

        // Best-effort legacy UMD version query. Some systems return unsupported.
        match adapter.CheckInterfaceSupport(&IDXGIDevice::IID) {
            Ok(v) => {
                let v = v as u64;
                eprintln!(
                    "ISSUE153 driver_version={}.{}.{}.{} raw={:#018X}",
                    (v >> 48) & 0xffff, (v >> 32) & 0xffff,
                    (v >> 16) & 0xffff, v & 0xffff, v
                );
            }
            Err(e) => eprintln!("ISSUE153 driver_version=unavailable HRESULT={:#010X}: {e}",
                e.code().0 as u32),
        }

        // 2. Format support. Query each format separately and never unwrap.
        let formats = [
            ("R16G16B16A16_TYPELESS", DXGI_FORMAT_R16G16B16A16_TYPELESS),
            ("R16G16B16A16_UNORM", DXGI_FORMAT_R16G16B16A16_UNORM),
            ("R16G16B16A16_FLOAT", DXGI_FORMAT_R16G16B16A16_FLOAT),
            ("B8G8R8X8_TYPELESS", DXGI_FORMAT_B8G8R8X8_TYPELESS),
            ("B8G8R8X8_UNORM", DXGI_FORMAT_B8G8R8X8_UNORM),
        ];
        let s1 = [
            (0x0000_0001, "BUFFER"), (0x0000_0010, "TEXTURE1D"),
            (0x0000_0020, "TEXTURE2D"), (0x0000_0040, "TEXTURE3D"),
            (0x0000_0080, "TEXTURECUBE"), (0x0000_0100, "SHADER_LOAD"),
            (0x0000_0200, "SHADER_SAMPLE"), (0x0000_1000, "MIP"),
            (0x0000_4000, "RENDER_TARGET"), (0x0000_8000, "BLENDABLE"),
            (0x0001_0000, "DEPTH_STENCIL"), (0x0004_0000, "MULTISAMPLE_RESOLVE"),
            (0x0008_0000, "DISPLAY"), (0x0010_0000, "CAST_WITHIN_BIT_LAYOUT"),
            (0x0020_0000, "MULTISAMPLE_RENDERTARGET"),
            (0x0200_0000, "TYPED_UNORDERED_ACCESS_VIEW"),
        ];
        let s2 = [
            (0x0000_0001, "UAV_ATOMIC_ADD"),
            (0x0000_0002, "UAV_ATOMIC_BITWISE_OPS"),
            (0x0000_0004, "UAV_ATOMIC_COMPARE_STORE_OR_COMPARE_EXCHANGE"),
            (0x0000_0008, "UAV_ATOMIC_EXCHANGE"),
            (0x0000_0010, "UAV_ATOMIC_SIGNED_MIN_OR_MAX"),
            (0x0000_0020, "UAV_ATOMIC_UNSIGNED_MIN_OR_MAX"),
            (0x0000_0040, "UAV_TYPED_LOAD"), (0x0000_0080, "UAV_TYPED_STORE"),
            (0x0000_0100, "OUTPUT_MERGER_LOGIC_OP"), (0x0000_0200, "TILED"),
            (0x0000_4000, "MULTIPLANE_OVERLAY"),
        ];
        let decode = |bits: u32, names: &[(u32, &str)]| -> String {
            names.iter().filter(|(mask, _)| bits & mask != 0)
                .map(|(_, name)| *name).collect::<Vec<_>>().join("|")
        };
        for (name, format) in formats {
            let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
                Format: format, Support1: D3D12_FORMAT_SUPPORT1(0),
                Support2: D3D12_FORMAT_SUPPORT2(0),
            };
            let hr = device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&mut support as *mut D3D12_FEATURE_DATA_FORMAT_SUPPORT).cast(),
                std::mem::size_of::<D3D12_FEATURE_DATA_FORMAT_SUPPORT>() as u32,
            );
            match hr {
                Ok(()) => eprintln!(
                    "ISSUE153 format={name} HRESULT=0x00000000 Support1={:#010X}[{}] Support2={:#010X}[{}]",
                    support.Support1.0, decode(support.Support1.0, &s1),
                    support.Support2.0, decode(support.Support2.0, &s2)
                ),
                Err(e) => eprintln!(
                    "ISSUE153 format={name} HRESULT={:#010X} Support1/2=unavailable: {e}",
                    e.code().0 as u32
                ),
            }
        }
    }
}
```

**Binding note:** это snippet под версию `windows 0.62.2` проекта. Перед запуском следует проверить точные generated signatures методов `EnumAdapterByLuid`, `CheckInterfaceSupport` и `CheckFeatureSupport` для установленного lockfile. У разных генераций Windows bindings optional pointers и HRESULT wrappers отличаются. Это compile-time адаптация bindings, не повод менять архитектуру эксперимента. В частности, `CheckInterfaceSupport` может быть недоступен/unsupported, что уже обработано как необязательная диагностика.

### Matrix helper

Следующая функция запускается на том же device, с неизменными heap properties и параметрами texture. Она не использует `unwrap()`, поэтому отказ одного варианта не прекращает следующие.

```rust
#[cfg(test)]
fn d3d12_issue153_creation_matrix(device: &ID3D12Device) {
    let cases = [
        ("A", "R16G16B16A16_TYPELESS+ALLOW_RENDER_TARGET",
            DXGI_FORMAT_R16G16B16A16_TYPELESS,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET),
        ("B", "R16G16B16A16_UNORM+ALLOW_RENDER_TARGET",
            DXGI_FORMAT_R16G16B16A16_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET),
        ("C", "R16G16B16A16_FLOAT+ALLOW_RENDER_TARGET",
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET),
        ("D", "R16G16B16A16_TYPELESS+NONE+COMMON",
            DXGI_FORMAT_R16G16B16A16_TYPELESS,
            D3D12_RESOURCE_FLAGS(0),
            D3D12_RESOURCE_STATE_COMMON),
        ("E", "B8G8R8X8_TYPELESS+ALLOW_RENDER_TARGET",
            DXGI_FORMAT_B8G8R8X8_TYPELESS,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET),
    ];

    for (id, label, format, flags, initial_state) in cases {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 32,
            Height: 32,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: flags,
        };
        eprintln!(
            "ISSUE153 CREATE {id} {label}: desc={{Dimension:{:?}, Alignment:{}, Width:{}, Height:{}, DepthOrArraySize:{}, MipLevels:{}, Format:{:?}({}), SampleDesc:{{Count:{},Quality:{}}}, Layout:{:?}, Flags:{:#010X}}} InitialState={:#010X} ClearValue=None",
            desc.Dimension, desc.Alignment, desc.Width, desc.Height,
            desc.DepthOrArraySize, desc.MipLevels, desc.Format, desc.Format.0,
            desc.SampleDesc.Count, desc.SampleDesc.Quality, desc.Layout,
            desc.Flags.0, initial_state.0
        );

        let mut resource = None;
        let result = unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                initial_state,
                None,
                &mut resource,
            )
        };

        match result {
            Ok(()) => {
                eprintln!("ISSUE153 CREATE {id} HRESULT=0x00000000 result=SUCCESS");
                drop(resource);
            }
            Err(e) => eprintln!(
                "ISSUE153 CREATE {id} HRESULT={:#010X} result=FAIL error={e}",
                e.code().0 as u32
            ),
        }
    }
}
```

Вариант D намеренно использует `COMMON`, а не `RENDER_TARGET`: без `ALLOW_RENDER_TARGET` нельзя сохранять RT initial state. `D3D12_RESOURCE_FLAGS(0)` означает отсутствие resource flags. Все остальные параметры описания одинаковы; optimized clear value — `None`, как в исходном тесте.

### InfoQueue snapshot: сообщения Debug Layer

Сразу после создания диагностического device (и до матрицы) получить `ID3D12InfoQueue`, очистить старые сообщения. После каждого варианта A–E снять новый snapshot. Для полного вывода используйте двухэтапный `GetMessage`: сначала запрос размера, затем буфер и сообщение. Буфер нужно выделять с выравниванием, достаточным для `D3D12_MESSAGE` (например, `Vec<usize>`, а не полагаться на alignment `Vec<u8>`).

Псевдокод последовательности:

```rust
let info: ID3D12InfoQueue = device.cast()?;
unsafe { info.ClearStoredMessages()?; }

// Immediately before each CreateCommittedResource call:
// unsafe { info.ClearStoredMessages()?; }
// Run exactly one matrix case.
// Immediately after that call:
let count = unsafe { info.GetNumStoredMessages() };
for index in 0..count {
    let mut bytes = 0usize;
    unsafe { info.GetMessage(index, None, &mut bytes)?; }
    let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
    unsafe { info.GetMessage(index, Some(message), &mut bytes)?; }
    let m = unsafe { &*message };
    let text = unsafe {
        std::ffi::CStr::from_ptr(m.pDescription.cast())
            .to_string_lossy().into_owned()
    };
    eprintln!(
        "ISSUE153 INFOQUEUE index={index} category={:?} severity={:?} id={} text={text}",
        m.Category, m.Severity, m.ID
    );
}
```

В проектном `windows 0.62.2` сигнатура `GetMessage` может быть проецирована с optional pointer как `Option<*mut D3D12_MESSAGE>`; если generated binding ожидает raw pointer, использовать `std::ptr::null_mut()` для первого вызова и `message` для второго. Смысл и порядок двух вызовов неизменны. Проверить фактическую сигнатуру по Cargo.lock перед сборкой.

Важно: `ID3D12InfoQueue` создаётся только если Debug Layer включён и cast успешен. Если interface недоступен, вывести это явно; не трактовать пустой лог как отсутствие ошибок validation layer.

---

## 5. Точный порядок запуска и Debug Layer

Уже реализованный порядок в `create()`:

1. Build diagnostics binary/test с feature `diagnostics`.
2. До `D3D12CreateDevice` установить `VICTORY_D3D12_DEBUG=1`.
3. Вызвать `D3D12GetDebugInterface`.
4. При успехе вызвать `ID3D12Debug::EnableDebugLayer()`.
5. Только затем вызвать `D3D12CreateDevice`.
6. После создания устройства выполнить `device.cast::<ID3D12InfoQueue>()`.
7. Очистить InfoQueue перед каждым независимым CreateCommittedResource вызовом и прочитать сообщения сразу после него.

Вызов `EnableDebugLayer` после шага 5 недопустим: Microsoft документирует, что это приведёт к device removal.

Для данного эксперимента не включать `VICTORY_D3D12_GBV`: GPU-based validation предназначена для GPU work/command validation и не нужна, чтобы диагностировать аргументы CPU-side CreateCommittedResource. Не включать `VICTORY_NO_TDR`: оно не связано с проверяемым HRESULT и меняет поведение watchdog.

---

## 6. Команды воспроизведения

Запуск из PowerShell в корне репозитория на Windows:

```powershell
$env:VICTORY_D3D12_DEBUG = "1"
Remove-Item Env:VICTORY_D3D12_WARP -ErrorAction SilentlyContinue
cargo test -p victory-gpu --lib --all-features present_conversion_requires_typed_view_and_shader_resource_access -- --exact --nocapture
```

Если полный test name не принимается фильтром `--exact`, использовать без `--exact`:

```powershell
cargo test -p victory-gpu --lib --all-features present_conversion_requires_typed_view_and_shader_resource_access -- --nocapture
```

В тесте локально заменить вызов исходного `make(...).unwrap()` на диагностический путь либо вызвать отдельный экспериментальный test, который:
- создаёт свежий device через текущий `create()`;
- выводит adapter identity и feature level;
- запрашивает пять форматов;
- выполняет матрицу A–E;
- снимает InfoQueue после каждого случая;
- не делает assertions на успех любого CreateCommittedResource и не меняет/не отключает исходный тест.

Отдельный WARP прогон:

```powershell
$env:VICTORY_D3D12_DEBUG = "1"
$env:VICTORY_D3D12_WARP = "1"
cargo test -p victory-gpu --lib --all-features issue153_d3d12_diagnostics -- --nocapture
```

После запуска вернуть переменные окружения в исходное состояние. Для физического/default adapter повторить без `VICTORY_D3D12_WARP`.

Сохранение полного вывода:

```powershell
cargo test -p victory-gpu --lib --all-features issue153_d3d12_diagnostics -- --nocapture 2>&1 |
    Tee-Object -FilePath issue-153-d3d12-diagnostics.log
```

---

## 7. Интерпретация матрицы

| Наблюдение | Следующий вывод, который допустим |
|---|---|
| A fails; B/C pass; D passes; E passes | Изолирована разница именно typeless+RT пути; нужна конкретная InfoQueue запись. Это ещё не доказательство ошибки драйвера или нарушения спецификации. |
| A/B/C fail; D passes; E passes | Проблема связана с RT usage семейства R16 либо capability report/adapter; сопоставить typed Support1 RENDER_TARGET и HRESULT. |
| A/B/C/D fail; E passes | Проверить общие поля resource description, heap properties и adapter/runtime. Нельзя списывать только на RT flag, поскольку D не использует его. |
| A/B/C/D/E fail | Вероятна более общая проблема device/heap/runtime или некорректный контекст запуска. Нужен InfoQueue и проверка валидности самого device. |
| A passes в новом запуске, но исходный CI fail | Сначала сравнить adapter LUID, vendor/device/revision, driver version, feature level, OS/runner image и flags. Различие запуска может быть важнее изменения Debug Layer. |
| Format query fails | Зафиксировать HRESULT query; Support1/Support2 не интерпретировать как нулевую поддержку. |
| Query успешен, typed UNORM/FLOAT Support1 содержит RENDER_TARGET, но A fails | Проверить debug-layer message и resource desc. Capability bits повышают приоритет проверки typeless resource validation, но сами не доказывают root cause. |

Для RT resource наиболее релевантны `TEXTURE2D` и `RENDER_TARGET` в Support1. Для последующего SRV sampling полезен `SHADER_SAMPLE`. `CAST_WITHIN_BIT_LAYOUT` следует логировать как дополнительную характеристику, но не подменять им правило typeless compatibility.

---

## 8. Hosted runner: ограничения и риски

Подтверждённый прошлым отчётом контекст проблемного запуска: Windows Server 2025, hosted image `windows-2025-vs2026`, Azure, регион centralus. Эти данные относятся к зафиксированному run `45857648756`, а не гарантируют идентичность будущего runner.

Ограничения:

- GitHub-hosted runner может предоставлять программный/виртуальный adapter либо аппаратно-ускоренный путь, отличающийся от пользовательской машины. Нельзя заранее называть его WARP без adapter identity и выбранного пути.
- Набор adapter/driver может изменяться между запусками образа. Сравнение только по имени Windows runner недостаточно.
- Наличие feature level 12.0 не доказывает одинаковые возможности всех форматов и операций в конкретном runtime/driver.
- Debug Layer может отсутствовать как optional Windows Graphics Tools component. Ошибка `D3D12GetDebugInterface` должна быть записана отдельно; это не равнозначно успешному запуску без сообщений.
- WARP — полезный software control, но не эквивалент физического NVIDIA/AMD/Intel GPU. Успех на WARP не снимает вопрос о поведении физического драйвера.
- Hosted runner не позволяет достоверно проверить пользовательские физические GPU, OEM driver package, display topology или конкретный vendor-specific path.
- Если тестовый процесс не получает доступ к InfoQueue, надо сохранить HRESULT cast/query, а не выводить «Debug Layer ничего не сообщил».

Рекомендуемые среды сравнения: исходный hosted runner; WARP на том же runner; минимум один физический GPU с записанной версией драйвера. При доступности — разные vendor adapters.

---

## 9. Риски эксперимента

- **Изменение production поведения:** исключено, если snippet остаётся под `#[cfg(test)]` и не меняет `create()`, `Host`, resource path или CI.
- **Прерывание матрицы:** исключено, если нет `unwrap()`/assert на отдельных CreateCommittedResource результатах.
- **Смешивание сообщений:** предотвращается `ClearStoredMessages` перед каждым вариантом и чтением сразу после него.
- **Неверная интерпретация driver version:** CheckInterfaceSupport — best-effort legacy UMD query, его отсутствие не означает отсутствие драйвера.
- **Неверная интерпретация software adapter:** software flag сам по себе не тождественен WARP.
- **Параллельные D3D12 сообщения:** использовать отдельный freshly-created device только для эксперимента; не выполнять одновременно другие GPU тесты в этом процессе. InfoQueue message queue относится к конкретному device, но многопоточные сообщения на одном device могут перемешиваться.
- **Семантическая подмена:** не заменять typeless ресурс на UNORM/FLOAT в исходном тесте на основании одной только матрицы. Матрица — диагностика, а не исправление.
- **Обязательные capability assumptions:** не объявлять root cause по отсутствию одного бита без сопоставления конкретного format, feature level и D3D12 validation message.

---

## 10. Последующие действия

1. Согласовать локальный test-only запуск на Windows с указанным snippet.
2. Убедиться, что Windows bindings из `Cargo.lock` действительно соответствуют `windows 0.62.2`; адаптировать только синтаксис optional pointer/HRESULT projection, если generated signatures отличаются.
3. Выполнить hardware/default adapter запуск с `VICTORY_D3D12_DEBUG=1`.
4. Выполнить WARP запуск отдельно.
5. Сохранить полный stdout/stderr, adapter identity, format queries, matrix HRESULTs и InfoQueue messages.
6. Сопоставить результаты с проблемным CI run `45857648756`.
7. Только после этого подготовить отдельный root-cause analysis. Любой production fix должен ссылаться на конкретный capability/validation evidence и иметь maintainer review.
8. Не менять `dev` напрямую, не закрывать Issue #153 и не отключать тест.

---

## 11. Ссылки и источники

### Репозиторий

- [Issue #153](https://github.com/vanya890/VIctory/issues/153)
- [Предыдущий отчёт](https://github.com/vanya890/VIctory/blob/dev/docs/agent-reports/issue-153-d3d12-investigation.md)
- [host.rs в dev](https://github.com/vanya890/VIctory/blob/dev/crates/victory-gpu/src/host.rs)
- [Cargo.toml victory-gpu](https://github.com/vanya890/VIctory/blob/dev/crates/victory-gpu/Cargo.toml)
- [CI workflow](https://github.com/vanya890/VIctory/blob/dev/.github/workflows/ci.yml)
- [Проблемный CI run #45857648756](https://github.com/vanya890/VIctory/actions/runs/45857648756)

### Microsoft Learn / DirectX

- [D3D12_FEATURE_DATA_FORMAT_SUPPORT](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_feature_data_format_support)
- [D3D12_FORMAT_SUPPORT1](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_format_support1)
- [D3D12_FORMAT_SUPPORT2](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_format_support2)
- [ID3D12Device::CheckFeatureSupport](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device-checkfeaturesupport)
- [IDXGIAdapter1::GetDesc1](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiadapter1-getdesc1)
- [DXGI_ADAPTER_DESC1](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ns-dxgi-dxgi_adapter_desc1)
- [IDXGIFactory4::EnumAdapterByLuid](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_4/nf-dxgi1_4-idxgifactory4-enumadapterbyluid)
- [IDXGIAdapter::CheckInterfaceSupport](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiadapter-checkinterfacesupport)
- [ID3D12Debug::EnableDebugLayer](https://learn.microsoft.com/en-us/windows/win32/api/d3d12sdklayers/nf-d3d12sdklayers-id3d12debug-enabledebuglayer)
- [ID3D12InfoQueue::GetMessage](https://learn.microsoft.com/en-us/windows/win32/api/d3d12sdklayers/nf-d3d12sdklayers-id3d12infoqueue-getmessage)
- [windows crate 0.62.2](https://docs.rs/crate/windows/0.62.2)

### Итоговая позиция

На текущем этапе установлено место отказа, но не первопричина. Минимальный корректный эксперимент — пять format support queries, adapter identity, фактический feature level, best-effort driver version, пять независимых CreateCommittedResource вызовов и InfoQueue snapshot вокруг каждого вызова. Все действия ограничиваются test-only/diagnostics-only контекстом. Production-код, CI workflow, исходный тест и ветка `main` остаются неизменными.
