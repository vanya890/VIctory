# Issue #153: расследование D3D12 Present Plane test

**Статус:** расследование выполнено до предела доступных данных; точная первопричина пока не доказана.  
**Дата исследования:** 2026-11-26  
**Репозиторий:** [vanya890/VIctory](https://github.com/vanya890/VIctory)  
**Целевая ветка сохранения:** dev  
**Исследуемый CI-код:** PR #152, merge commit f92d664c91f271bfaee9bb4c448c87e032f4e637  
**Базовый commit PR:** c619e15c61d1af13e13c21cc2a86ba42b54b109c  
**Head commit PR:** 2510222470cdff65da5ab6ee08e96c7422e1485c

> Исходники не изменялись. Изменения CI, Issue, PR, комментарии, ветки разработки и слияния не выполнялись. Этот отчёт — единственное изменение репозитория по отдельному разрешению владельца.

---

## 1. Краткий вывод

### Установлено

1. HRESULT 0x80070057 возвращает **ID3D12Device::CreateCommittedResource**, вызванный из тестовой closure make.
2. Ошибка возникает на первой попытке создать ресурс с:
   - Format = DXGI_FORMAT_R16G16B16A16_TYPELESS (9);
   - Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET (1);
   - InitialResourceState = D3D12_RESOURCE_STATE_RENDER_TARGET.
3. Падение происходит до CreateRenderTargetView, CreateShaderResourceView, создания compositor pipeline state, записи команд и resource barriers.
4. В той же ветке есть контрольный тест, который успешно создаёт typeless BGRX resource с теми же heap properties, размерами, sample description, RT flag, initial state и отсутствующим optimized clear value. Общие параметры committed resource сами по себе не выглядят причиной; различие сосредоточено в формате/его поддержке на выбранном устройстве.
5. По документации Microsoft комбинация typeless resource + ALLOW_RENDER_TARGET не является безусловно запрещённой: для typeless формата допускается RT-флаг, если формат из той же typeless-группы поддерживает render-target capability. Для R16G16B16A16 группа содержит typed FLOAT и UNORM, оба с RT support на Feature Level 12.0.
6. В коде и CI отсутствует диагностический вывод D3D12_FEATURE_FORMAT_SUPPORT для этой группы, а D3D12 debug layer в проблемном запуске не был явно включён.

### Не установлено

**Нельзя честно назвать конкретный аргумент нарушающим спецификацию.** Наиболее вероятная область проблемы — фактическая поддержка/валидация формата R16G16B16A16_TYPELESS с RT usage конкретным D3D12 adapter/driver в Windows CI либо несоответствие драйвера таблице возможностей. Но доступные логи не содержат имени адаптера, драйвера, результатов format-support queries или debug-layer сообщения.

Иными словами: API-вызов установлен; точная причина E_INVALIDARG — пока нет.

### Рекомендация

Не менять production resource flags и не заменять typeless ресурс typed форматом вслепую. Следующий корректный шаг — получить format-support bitmasks для R16G16B16A16_UNORM и R16G16B16A16_FLOAT, включить D3D12 debug layer и выполнить небольшую матрицу создания ресурсов, меняя только формат и RT-флаг. Это отделит недоступную возможность адаптера от ошибки в тестовой конфигурации или драйверной реализации.

---

## 2. Исследованная версия проекта

| Поле | Значение |
|---|---|
| Репозиторий | https://github.com/vanya890/VIctory |
| Ветка, на которой находится проблема | dev |
| PR | [#152](https://github.com/vanya890/VIctory/pull/152) |
| Issue | [#153](https://github.com/vanya890/VIctory/issues/153) |
| CI run | [45857648756](https://github.com/vanya890/VIctory/actions/runs/45857648756) |
| PR base | dev, SHA c619e15c61d1af13e13c21cc2a86ba42b54b109c |
| PR head | agent/ci-coverage, SHA 2510222470cdff65da5ab6ee08e96c7422e1485c |
| PR merge commit | f92d664c91f271bfaee9bb4c448c87e032f4e637 |
| Исследованный файл | crates/victory-gpu/src/host.rs |
| Blob SHA файла в актуальном dev на момент чтения | abae5d777dc6325673f2c26de518f9657199b4ec |
| CI workflow | .github/workflows/ci.yml |
| Дата CI run | 2026-11-26 |

### Важное расхождение веток

Текущая default branch форка — main. На момент исследования:

- main не содержит тест present_conversion_requires_typed_view_and_shader_resource_access;
- dev содержит этот тест;
- PR #152 направлен в dev, а не в main;
- upstream Marvisak/VIctory также использует default branch main, где этот конкретный тест отсутствует.

Поэтому падение исследуется в контексте PR merge commit f92d664... и базы dev, а не как дефект текущего main.

---

## 3. Точная последовательность выполнения теста

Тест расположен в crates/victory-gpu/src/host.rs, строки **4750–4895** в исследованной версии.

### Последовательность

1. Вызывается host() (строка 4751). Если D3D12 device создать не удалось, тест завершает выполнение без проверки. В CI тест не пропускается: он дошёл до resource creation.
2. Создаётся closure make(format, flags) (строка 4755).
3. Closure вызывает host.device.CreateCommittedResource(...) (строки 4759–4780).
4. HRESULT преобразуется Rust Windows binding в Result; на строке **4782** вызывается unwrap().
5. Именно этот unwrap паникует с HRESULT(0x80070057).
6. Следующая строка теста должна была создать ресурс make(DXGI_FORMAT_R16G16B16A16_TYPELESS, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET) (строки 4786–4789).
7. После успешного создания ресурс должен был получить RTV с typed форматом DXGI_FORMAT_R16G16B16A16_UNORM (строки 4791–4800).
8. Затем выполняется clear, command list submission и вызов present_planes_with_compositor с PresentPlane::Raw и typed-view metadata (строки 4801–4814).
9. Compositor/SRV conversion и readback проверяются позже.

Фактически выполнение останавливается на шаге 4. Шаги 6–9 — ожидаемый сценарий теста, но они не выполняются в падающем запуске.

### Точная сигнатура вызова и аргументы

Вызов в Rust:

    host.device.CreateCommittedResource(
        &D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        },
        D3D12_HEAP_FLAG_NONE,
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 32,
            Height: 32,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: flags,
            ..Default::default()
        },
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        None,
        &mut texture,
    ).unwrap();

В первой попытке closure получает:

    format = DXGI_FORMAT_R16G16B16A16_TYPELESS
    flags  = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET

| Параметр API | Фактическое значение | Оценка |
|---|---|---|
| Heap Type | DEFAULT | Допустимо для GPU texture |
| Heap Flags | NONE | Допустимо |
| Dimension | TEXTURE2D | Допустимо |
| Width / Height | 32 / 32 | Допустимые ненулевые размеры |
| DepthOrArraySize | 1 | Допустимо |
| MipLevels | 1 | Допустимо |
| Format | R16G16B16A16_TYPELESS | Требует проверки RT support для typeless family |
| SampleDesc | Count=1, Quality=0 | Обычный single-sample ресурс |
| Layout | UNKNOWN (default) | Обычный layout для default texture |
| Flags | ALLOW_RENDER_TARGET | Требует RT capability у формата либо у typed member typeless family |
| Initial State | RENDER_TARGET | Согласуется с RT flag |
| Optimized Clear Value | None | Допустимо; это оптимизация, не обязательный параметр |

### Контрольное сравнение внутри того же файла

Тест present_primary_bgrx_uses_typed_view_and_integer_primary_is_rejected (строки 4651–4687) использует аналогичную closure:

- те же DEFAULT heap properties;
- тот же HEAP_FLAG_NONE;
- та же TEXTURE2D;
- 32×32, 1 array slice, 1 mip;
- тот же sample description;
- тот же ALLOW_RENDER_TARGET;
- то же начальное состояние RENDER_TARGET;
- None для optimized clear value.

Он успешно создаёт DXGI_FORMAT_B8G8R8X8_TYPELESS и затем typed RTV B8G8R8X8_UNORM.

Это сильное доказательство, что ошибка не объясняется общим шаблоном heap properties, отсутствующим clear value, размером, sample count или начальным состоянием. Существенное отличие в первой операции — DXGI format.

---

## 4. Техническая причина ошибки

### Подтверждено

- HRESULT возвращён именно из CreateCommittedResource.
- Ошибка возникает при создании первой текстуры формата R16G16B16A16_TYPELESS.
- RTV/SRV creation не выполнялись.
- E_INVALIDARG — это HRESULT 0x80070057, который CI логирует как “The parameter is incorrect.”

### Что говорят документы о формате

R16G16B16A16_TYPELESS имеет DXGI enum value 9. В той же typeless family находятся, в частности:

- R16G16B16A16_FLOAT (10);
- R16G16B16A16_UNORM (11).

Документация D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET прямо допускает два варианта: сам формат поддерживает RT, либо, если формат typeless, RT поддерживает формат из той же typeless group.

Для Direct3D Feature Level 12.0 таблица Microsoft указывает RT support как обязательный для R16G16B16A16_FLOAT и R16G16B16A16_UNORM. Поэтому утверждение «типизированный формат UNORM не может быть RT» не соответствует спецификации.

При этом в таблице у самой строки R16G16B16A16_TYPELESS поле RenderTarget помечено как недоступное. Это не опровергает исключение в описании resource flag: typeless resource должен использовать typed compatible view, а RT capability для resource flag определяется по typed member той же группы.

### Что нельзя заключить по текущим данным

Нельзя доказать, что R16G16B16A16_TYPELESS + ALLOW_RENDER_TARGET — заведомо недопустимая комбинация. Документация Microsoft описывает условия, при которых она допустима. Нельзя также доказать, что конкретный Windows runner реально предоставляет корректный FL 12.0 adapter и корректную поддержку всех обязательных форматов: в логах отсутствует идентификация адаптера и запросы его capabilities.

### Наиболее вероятная область причины

**Высоковероятная гипотеза:** format support / driver validation path для R16G16B16A16 на адаптере Windows CI. Это может быть фактическое отсутствие RT support у typed member на используемом adapter, некорректная реализация драйвера/виртуального адаптера или проблема специфической валидации typeless resource.

Это пока не установленная первопричина: для подтверждения необходимы результаты CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT) и сообщения debug layer.

### Почему это не shader или state bug

- CreateCommittedResource завершается ошибкой до создания descriptor views.
- CreateRenderTargetView вызывается только после успешного resource creation.
- CreateShaderResourceView вызывается внутри compositor path позднее.
- Compositor::new и компиляция шейдера выполняются после создания исходной текстуры и не могли породить HRESULT на строке 4782.
- Командный список, clear, submit, barriers и copy операции также расположены позже.

---

## 5. Анализ DXGI и D3D12

| Ресурс / формат | Формат | Флаги | Назначение | Результат проверки |
|---|---|---|---|---|
| Исходная тестовая текстура | R16G16B16A16_TYPELESS | ALLOW_RENDER_TARGET | Typed RTV + последующий shader sampling | **Фактически падает** при CreateCommittedResource; причина пока не локализована до конкретного capability bit |
| Ожидаемый RTV | R16G16B16A16_UNORM | View, не resource flag | Clear и интерпретация исходных данных как UNORM | Не достигнут в упавшем запуске; typed формат совместим с typeless family |
| Ожидаемый SRV | R16G16B16A16_UNORM | View, не resource flag | Shader sampling перед преобразованием в swapchain format | Не достигнут; view должен иметь typed compatible format |
| Контрольный ресурс | B8G8R8X8_TYPELESS | ALLOW_RENDER_TARGET | Typed BGRX RTV + compositor | Успешно создаётся в соседнем тесте |
| Typed контроль, отдельная часть теста | R16G16B16A16_UNORM | ALLOW_RENDER_TARGET | Проверка поведения compositor при ошибке создания | В тесте создаётся позже (строки 4838–4866), но до этого участка выполнение не доходит |

### Typed и typeless

Typeless resource — корректный способ отложить интерпретацию битов до создания typed view. View description должна задавать typed compatible format; использовать _TYPELESS непосредственно в RTV/SRV description нельзя.

В рассматриваемом тесте предполагаемая пара:

    Resource: R16G16B16A16_TYPELESS
    RTV:      R16G16B16A16_UNORM
    SRV:      R16G16B16A16_UNORM

По формату это совместимая семья. Вызов RTV/SRV не является источником текущего падения, поскольку он не был достигнут.

### Resource flags

- ALLOW_RENDER_TARGET позволяет создавать RTV и переходить в RENDER_TARGET.
- DENY_SHADER_RESOURCE не установлен, следовательно, ресурс не запрещает SRV.
- ALLOW_DEPTH_STENCIL и ALLOW_UNORDERED_ACCESS не установлены.
- По Microsoft, для typeless resource RT flag допустим, если typed format из той же группы поддерживает RT.

### Resource States

Начальное состояние RENDER_TARGET соответствует установленному ALLOW_RENDER_TARGET. Проверки переходов между RT и pixel shader resource не являются причиной HRESULT, поскольку resource creation не проходит до командной части теста.

### Format support

В реализации host device initialization найден CheckFeatureSupport для D3D12_FEATURE_D3D12_OPTIONS2 (depth bounds). Запроса D3D12_FEATURE_FORMAT_SUPPORT для R16G16B16A16 в этой точке нет. В самом тесте также нет format-support query перед CreateCommittedResource.

Это основной пробел диагностики.

---

## 6. Анализ Windows CI

### Что установлено по workflow и логам

Проблемный job: **Tests (all)**, job ID 140040137089.

- ОС: Microsoft Windows Server 2025, build 10.0.26100.
- Runner image: windows-2025-vs2026.
- Runner image version: 20261116.315.1.
- Облако: Azure; регион в логах — centralus.
- Rust toolchain в логе cache environment: 1.100.0-x86_64-pc-windows-msvc.
- Запускал cargo test --workspace --all-features --locked.
- Результат: 296 passed, 1 failed, 1 ignored.
- Точная ошибка: host.rs:4782:22, HRESULT 0x80070057.

Источники:
- [Workflow run #45857648756](https://github.com/vanya890/VIctory/actions/runs/45857648756)
- [PR #152](https://github.com/vanya890/VIctory/pull/152)
- [CI workflow в dev](https://github.com/vanya890/VIctory/blob/dev/.github/workflows/ci.yml)

### Выбор adapter

В host.rs, функция create() (около строк 144–260), устройство создаётся так:

    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;

Путь WARP существует, но включается только при одновременном выполнении:

- feature diagnostics;
- переключатель окружения VICTORY_D3D12_WARP.

В workflow для проблемного test job такого environment variable не задано. Следовательно, код не запрашивает WARP явно, а использует default adapter selection через D3D12CreateDevice(None, ...).

Логи не содержат имени адаптера, vendor/device ID, версии драйвера или факта, что выбранная реализация является физическим GPU, виртуальным GPU или software adapter. Поэтому нельзя утверждать, что CI использует WARP или конкретную модель GPU.

### Debug layer

В коде debug layer включается только при diagnostics и переключателе VICTORY_D3D12_DEBUG. Наличие --all-features само по себе не включает этот runtime switch. В CI workflow переменная не задана. Поэтому доступный лог не содержит D3D12 debug-layer объяснения некорректного аргумента.

### Вывод по CI

Подтверждено, что падение происходит в Windows Server 2025 hosted runner. Не подтверждено, что это проблема именно WARP, драйвера или физического GPU. Для этого нужны adapter identity и format-support diagnostics.

---

## 7. Сравнение с upstream

Проверен upstream Marvisak/VIctory, default branch main, файл crates/victory-gpu/src/host.rs.

- Тест present_conversion_requires_typed_view_and_shader_resource_access в upstream main отсутствует.
- В upstream main функция swap_format использует mapping диапазона 9..=14 к R16G16B16A16_FLOAT.
- В форке dev mapping уже более избирателен: 10 | 11 | 13 соответствуют R16G16B16A16_FLOAT; typeless/integer форматы не принимаются без дополнительного typed-view пути.
- В upstream main и fork main содержимое host.rs на момент чтения совпадало по blob SHA 3c2ed783c5fb823ccd654887f2b3d3b58fb0f322. Но исследуемая задача находится в dev, который отличается от main.

Ссылки:
- [Upstream host.rs](https://github.com/Marvisak/VIctory/blob/main/crates/victory-gpu/src/host.rs)
- [Fork dev host.rs](https://github.com/vanya890/VIctory/blob/dev/crates/victory-gpu/src/host.rs)

Найденного upstream commit/PR, который исправляет именно CreateCommittedResource для R16G16B16A16 typeless + RT flag, не обнаружено. Переносить изменения upstream без доказательства связи с этим HRESULT не следует.

---

## 8. Доказательства

### Исходный код

- crates/victory-gpu/src/host.rs:4750–4784 — тест и падающая closure.
- crates/victory-gpu/src/host.rs:4786–4789 — конкретный failing call: R16G16B16A16_TYPELESS + ALLOW_RENDER_TARGET.
- crates/victory-gpu/src/host.rs:4791–4800 — RTV создаётся после неуспешной строки, значит не является причиной.
- crates/victory-gpu/src/host.rs:4807–4834 — compositor conversion и readback после resource creation.
- crates/victory-gpu/src/host.rs:4651–4687 — контрольный B8G8R8X8 typeless ресурс с аналогичными остальными параметрами.
- crates/victory-gpu/src/host.rs:144–260 — создание D3D12 device, WARP switch и debug-layer switch.
- crates/victory-gpu/src/host.rs:2794–2803 — mapping swapchain formats.
- crates/victory-gpu/src/host.rs:3405–3469 — подготовка compositor path; вызывается позже.
- crates/victory-gpu/src/resource.rs:1182–1210 — преобразование host resource descriptor; не является стеком текущего теста.

### CI лог

В job Tests (all):

    test host::present_plane_tests::present_conversion_requires_typed_view_and_shader_resource_access ... FAILED

    thread 'host::present_plane_tests::present_conversion_requires_typed_view_and_shader_resource_access'
    panicked at crates\victory-gpu\src\host.rs:4782:22:

    called Result::unwrap() on an Err value:
    Error {
      code: HRESULT(0x80070057),
      message: "The parameter is incorrect."
    }

    test result: FAILED. 296 passed; 1 failed; 1 ignored

### Официальная документация Microsoft

1. [D3D12_RESOURCE_FLAGS](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_resource_flags) — ограничения ALLOW_RENDER_TARGET; для typeless resource capability должна поддерживаться форматом из той же typeless group.
2. [DXGI Format Support for Direct3D Feature Level 12.0](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/hardware-support-for-direct3d-12-0-formats) — R16G16B16A16_FLOAT и R16G16B16A16_UNORM имеют обязательный RT support на FL12.0; сама typeless строка не является typed RTV format.
3. [D3D12_RESOURCE_DESC](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_resource_desc) — требования к dimensions, alignment, mip levels, format и sample description.
4. [D3D12_SHADER_RESOURCE_VIEW_DESC](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_shader_resource_view_desc) — SRV должен задавать typed compatible view format; typeless format непосредственно в view description не используется.
5. [D3D12_RENDER_TARGET_VIEW_DESC](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_render_target_view_desc) — RTV не может использовать typeless view format.
6. [D3D12_FEATURE_DATA_FORMAT_SUPPORT](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_feature_data_format_support) — структура фактических format support bits драйвера.
7. [ID3D12Device::CheckFeatureSupport](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device-checkfeaturesupport) — API запроса возможностей текущего графического драйвера.
8. [ID3D12Device::CreateCommittedResource](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device-createcommittedresource) — параметры resource creation и optional optimized clear value.
9. [D3D12_HEAP_PROPERTIES](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_heap_properties) — нулевые CreationNodeMask/VisibleNodeMask эквивалентны единице на single-GPU adapter, поэтому default masks не являются убедительной причиной данного падения.
10. [D3D12_FORMAT_SUPPORT1](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_format_support1) — значения TEXTURE2D, SHADER_SAMPLE, RENDER_TARGET и других capability bits.

---

## 9. Рекомендуемое исправление / следующий диагностический шаг

Пока не рекомендую менять production implementation: доступные данные не доказывают, что её поведение неверно. Сначала требуется минимальная диагностическая доработка теста (локальный эксперимент или отдельное согласованное изменение).

### 9.1. Запросить реальные возможности адаптера

До создания ресурса запросить через:

CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, ...)

минимум для:

- DXGI_FORMAT_R16G16B16A16_UNORM;
- DXGI_FORMAT_R16G16B16A16_FLOAT;
- DXGI_FORMAT_R16G16B16A16_TYPELESS;
- DXGI_FORMAT_B8G8R8X8_UNORM как контрольного typed member.

Сохранить в лог:

- HRESULT самого query;
- Support1 и Support2 в hex;
- наличие TEXTURE2D, RENDER_TARGET, SHADER_SAMPLE, CAST_WITHIN_BIT_LAYOUT;
- имя адаптера, vendor ID, device ID, driver version;
- фактически используемый feature level.

Не проверять RT bit на typeless view как будто это typed RTV: для resource flag применяется правило typeless group, а typed view проверяется отдельно.

### 9.2. Матрица изолирующих resource creation checks

На одном device, с одинаковыми heap properties, размерами, sample description и state, выполнить по отдельности:

| Вариант | Resource format | Flags | Что изолирует |
|---|---|---|---|
| A | R16G16B16A16_TYPELESS | ALLOW_RENDER_TARGET | Точное воспроизведение падения |
| B | R16G16B16A16_UNORM | ALLOW_RENDER_TARGET | Поддержку typed UNORM RT resource |
| C | R16G16B16A16_FLOAT | ALLOW_RENDER_TARGET | Поддержку typed FLOAT RT resource |
| D | R16G16B16A16_TYPELESS | NONE | Возможность создать typeless texture без RT capability |
| E | B8G8R8X8_TYPELESS | ALLOW_RENDER_TARGET | Положительный контроль из соседнего теста |

Для варианта D начальное состояние должно быть COMMON, а не RENDER_TARGET, поскольку RT flag отсутствует.

Каждый вызов должен логировать отдельный HRESULT и полный D3D12_RESOURCE_DESC. Не объединять вызовы в один unwrap(): иначе снова потеряется точка отказа.

### 9.3. Включить D3D12 debug layer

Для отдельного диагностического запуска установить VICTORY_D3D12_DEBUG=1 при all-features test run. Код create() уже содержит opt-in включение debug layer при feature diagnostics. Сохранить сообщения debug layer вокруг failing resource creation.

Не менять постоянную конфигурацию CI без согласования. Сначала выполнить диагностический запуск в отдельном эксперименте.

### 9.4. Как интерпретировать результаты

- **B и C падают, D проходит:** вероятна проблема capability RT у typed formats или adapter/driver; сверить support bits и заявленный FL.
- **B и C проходят, D проходит, A падает:** изолирована проблема typeless + RT комбинации в runtime/driver; сопоставить с debug-layer сообщением и спецификацией.
- **B/C/D/A проходят после включения debug layer:** проверить, не было ли различия в выбранном adapter/device между запусками; debug layer сам по себе не должен менять корректность параметров.
- **B проходит, C падает:** проверить фактическую поддержку FLOAT и UNORM отдельно; нельзя выводить поддержку одного typed member из поддержки другого.
- **Все форматы кроме B падают:** перепроверить полный resource description и устройство, а не только формат; тогда контрольный тест мог использовать другой device/process/context.

### 9.5. Возможный production fix

На текущем этапе **production fix не обоснован**. Если format-support query подтвердит, что adapter не поддерживает нужный формат/операцию, корректное решение должно быть capability-aware: выбрать допустимый typed resource/view path или сообщить о недоступности конкретного режима. Нельзя маскировать ошибку, отключая тест или игнорируя HRESULT.

Если capability query подтвердит обязательные FL12.0 bits, а debug layer покажет invalid resource description, исправлять нужно конкретное поле, названное validation layer. До получения этих данных diff не предлагается.

---

## 10. Риски и регрессии

- Замена typeless resource на R16G16B16A16_UNORM может убрать возможность выбирать другой typed interpretation в будущем; делать это без проверки семантики нельзя.
- Замена на R16G16B16A16_FLOAT изменит интерпретацию исходных 16-bit channel values и может разрушить тестируемую UNORM-to-FLOAT conversion.
- Удаление ALLOW_RENDER_TARGET недопустимо, пока тест очищает ресурс через RTV.
- Добавление DENY_SHADER_RESOURCE прямо противоречит назначению теста.
- Пропуск теста по имени адаптера или без capability evidence скроет проблему и не является исправлением.
- Неправильный fallback format может дать зелёный тест, но перестать проверять реальный Present Plane conversion path.

---

## 11. План тестирования

После диагностической доработки и последующего согласованного исправления:

1. На Windows выполнить только:
   cargo test -p victory-gpu --lib present_conversion_requires_typed_view_and_shader_resource_access -- --exact --nocapture
   (при необходимости скорректировать фильтр под полное имя теста).
2. Повторить с --all-features и VICTORY_D3D12_DEBUG=1.
3. Выполнить контрольные тесты:
   - present_primary_bgrx_uses_typed_view_and_integer_primary_is_rejected;
   - остальные present_plane_tests;
   - present_format_tests.
4. Выполнить cargo test --workspace --locked.
5. Выполнить cargo test --workspace --all-features --locked.
6. Выполнить cargo check --workspace --all-targets --locked и all-features variant.
7. Выполнить Release build:
   cargo build --release -p victory --locked.
8. Выполнить Diagnostics build:
   cargo build --profile diag -p victory --features diagnostics --locked.
9. Выполнить cargo clippy --workspace --all-targets --locked -- -D warnings и all-features variant.
10. Выполнить cargo fmt --all -- --check и документацию, если изменения затронут соответствующие файлы.

### CI и физический GPU

- Windows hosted CI может проверить повторяемость на том же runner image, но для интерпретации нужен adapter identity.
- WARP можно проверить отдельным diagnostics запуском с явным VICTORY_D3D12_WARP=1, если он поддерживает требуемый feature level и формат.
- Для реального GPU требуется как минимум один физический adapter; желательно отдельно проверить NVIDIA и AMD.
- Текущий workflow не доказывает physical-GPU coverage. PR #152 прямо оговаривает отсутствие physical-GPU и end-to-end gameplay validation.

---

## 12. Что осталось неизвестным

1. Имя и PCI ID D3D12 adapter на runner windows-2025-vs2026.
2. Версия драйвера и фактический runtime path.
3. Результаты CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT) для R16G16B16A16 typed members.
4. Debug-layer сообщение, объясняющее отказ resource creation.
5. Результат изолированной матрицы typed/typeless resource creation на том же adapter.
6. Воспроизводится ли ошибка на физическом GPU и WARP.
7. Является ли причина нарушением в тестовой конфигурации, ограничением конкретного adapter или ошибкой драйвера.

Для окончательного заключения достаточно начать с пунктов 1–5. До этого утверждать точный invalid argument нельзя.

---

## 13. Передача основному разработчику

**Что уже известно:** падает CreateCommittedResource в crates/victory-gpu/src/host.rs:4759–4782, на первой попытке создать R16G16B16A16_TYPELESS с ALLOW_RENDER_TARGET и начальным RENDER_TARGET.

**Что не нужно исправлять:** RTV, SRV, shader cache, compositor pipeline, resource barriers и copy path не являются непосредственным источником данного HRESULT: выполнение до них не доходит.

**Что сделать следующим:** получить D3D12_FEATURE_FORMAT_SUPPORT для R16G16B16A16_UNORM/FLOAT, включить debug layer, вывести adapter identity и прогнать матрицу A–E из раздела 9.

**Почему нельзя пока дать production diff:** документация Microsoft разрешает RT resource flag для typeless формата, когда typed member той же группы поддерживает RT. На FL12.0 typed UNORM/FLOAT имеют обязательную RT поддержку. Следовательно, простой совет «заменить typeless на typed» не доказан и может изменить смысл теста.

**Как проверить:** повторить проблемный тест с диагностикой, затем all-features workspace tests, default tests, builds, Clippy и форматирование.

**Дальнейшие действия:** после получения capability bits и debug-layer output определить, следует ли исправлять test setup, device selection/capability handling или конкретное поле resource description. Не отключать тест и не подавлять HRESULT.

---

## Источники и ссылки

- [Issue #153](https://github.com/vanya890/VIctory/issues/153)
- [PR #152](https://github.com/vanya890/VIctory/pull/152)
- [Workflow run #45857648756](https://github.com/vanya890/VIctory/actions/runs/45857648756)
- [Исследуемый host.rs в dev](https://github.com/vanya890/VIctory/blob/dev/crates/victory-gpu/src/host.rs)
- [CI workflow в dev](https://github.com/vanya890/VIctory/blob/dev/.github/workflows/ci.yml)
- [Upstream host.rs](https://github.com/Marvisak/VIctory/blob/main/crates/victory-gpu/src/host.rs)
- [Microsoft: D3D12_RESOURCE_FLAGS](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_resource_flags)
- [Microsoft: DXGI Format Support FL12.0](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/hardware-support-for-direct3d-12-0-formats)
- [Microsoft: D3D12_RESOURCE_DESC](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_resource_desc)
- [Microsoft: SRV description](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_shader_resource_view_desc)
- [Microsoft: RTV description](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_render_target_view_desc)
- [Microsoft: D3D12_FEATURE_DATA_FORMAT_SUPPORT](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_feature_data_format_support)
- [Microsoft: CheckFeatureSupport](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device-checkfeaturesupport)
- [Microsoft: CreateCommittedResource](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device-createcommittedresource)
- [Microsoft: D3D12_HEAP_PROPERTIES](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_heap_properties)
