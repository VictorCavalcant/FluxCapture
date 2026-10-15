# FluxCapture

Gravador de tela para Windows 10/11 escrito em Rust, feito para interferir o mínimo possível no que
está sendo gravado. A captura, a composição da cena, a conversão de cor e a codificação acontecem
**inteiramente na GPU**: nenhum quadro passa pela memória do sistema no caminho da gravação.

| Medido nesta máquina (Ryzen 5 4600G + RTX 3050, build release, 1080p60) | H.264 (NVENC) | HEVC (NVENC) |
|---|---|---|
| CPU do processo (média, % da máquina) | **1,07%** | **1,08%** |
| Composição por quadro: CPU / GPU | 0,15 ms / 0,21 ms | 0,18 ms / 0,80 ms |
| Latência do encoder | 5,4 ms | 7,3 ms |
| Quadros perdidos em 20 s | 0 | 0 |
| Memória (private bytes, pico) | 173 MB | 171 MB |

Como reproduzir: `fluxcapture-cli bench --seconds 20 --json --codec h264 --res 1080p --fps 60`
(metodologia e comparação com o OBS em [docs/performance.md](docs/performance.md)).

## Recursos

- **Captura**: tela inteira / monitor, janela (mesmo atrás de outras ou minimizada mantém o último
  quadro), região (selecionada arrastando na prévia), webcam / placa de captura. Windows.Graphics.Capture
  por padrão; DXGI Desktop Duplication como alternativa (cursor desenhado pelo compositor).
- **Cenas**: várias cenas com camadas (tela, janela, região, imagem, texto, webcam, cor), mover e
  redimensionar direto na prévia, recorte, opacidade, ajuste (conter / preencher / esticar / original),
  filtros de escala bilinear, bicúbico ou Lanczos com mipmaps para reduções grandes.
- **Vídeo**: H.264 e HEVC pelos encoders de hardware do Windows (NVIDIA NVENC, AMD AMF, Intel Quick Sync)
  via Media Foundation, com fallback por software; qualidade constante, VBR ou CBR; preset; intervalo
  de keyframe; modo de baixa latência; taxa de quadros constante ou **variável** (não codifica quadros
  repetidos de uma tela parada); BT.709 limitado ou completo.
- **Áudio**: som do sistema (loopback), microfones, qualquer dispositivo, ou **somente o som de um
  aplicativo** (Windows 11). Mixer com volume, mudo, balanço, mono, atraso de sincronia, filtros
  (passa-altas, supressão de ruído, noise gate, compressor), limitador no master, medidores de
  pico/RMS com aviso de clipping. Trilhas separadas no arquivo (mixagem + uma por fonte). Correção de
  deriva de relógio entre os dispositivos e o relógio de vídeo (sincronia estável por horas).
- **Gravação segura**: Matroska gravado de forma que uma queda do PC deixa o arquivo reproduzível
  (perde no máximo ~1 s); journal de sessão e **recuperação** na próxima abertura, sem tocar no
  original; remux para MP4 ao terminar (sem recodificar); divisão por tempo ou tamanho; aviso de pouco
  espaço e finalização segura antes de o disco encher; pausa sem buraco no arquivo.
- **Replay instantâneo**: os últimos N segundos (já codificados) em memória com limite de RAM;
  salvar é imediato (MP4 ou MKV). Gravação e replay usam o **mesmo** encode.
- **Diagnóstico**: indica se o gargalo é captura, composição (CPU/GPU), encoder, disco, áudio ou
  sistema, com a métrica observada e a sugestão. O *Auto Performance* propõe ajustes de um clique e
  nunca muda a saída sem confirmação (parâmetros travados no perfil nunca são sugeridos); sob
  sobrecarga, só reduz a prévia — e avisa.
- **Assistente de configuração**: mede o disco e faz um teste real de gravação com cada encoder de
  hardware, recomendando resolução, FPS, codec e taxa de bits (com GB/hora).
- **Perfis**: Gaming, Qualidade, Baixo consumo e Personalizado; duplicar, exportar e importar.
- **Prévia**: taxa e tamanho ajustáveis; opção de **alta qualidade** (resolução quase nativa, filtro
  Lanczos), removida enquanto grava para não pesar no encode.
- **Uso diário**: atalhos globais (gravar, pausar, ativar/desativar e salvar replay, captura de tela,
  microfone mudo),
  ícone na bandeja, iniciar minimizado, janelas do FluxCapture ocultas das próprias gravações,
  português e inglês, tema claro/escuro, modo portátil, logs exportáveis sem segredos, canal de
  controle JSON local (automação).

## Instalação (para quem só quer usar)

1. Baixe `FluxCapture-Setup-<versão>.exe` e dê dois cliques nele.
2. Se o Windows mostrar "O Windows protegeu o computador", clique em **Mais informações** e depois em
   **Executar assim mesmo** (o instalador ainda não tem assinatura digital).
3. Clique em **Instalar**. Não é preciso senha de administrador.

O FluxCapture fica no Menu Iniciar (e na Área de Trabalho, se você deixar marcado). Para remover:
**Configurações › Aplicativos › Aplicativos instalados › FluxCapture › Desinstalar**. As gravações
nunca são apagadas; as configurações só se você marcar essa opção. Para atualizar, basta rodar o
instalador da versão nova.

Instalação sem janela (TI / scripts): `FluxCapture-Setup-<versão>.exe --silent [--dir PASTA]
[--no-desktop-shortcut]`; remoção: `uninstall.exe --silent [--remove-settings]`.

## Uso

```bash
cargo run --release -p fluxcapture
```

```bash
cargo run --release -p fluxcapture-cli -- devices
```

```bash
cargo run --release -p fluxcapture-cli -- record --seconds 30 --codec hevc --mp4
```

A CLI também faz `replay`, `bench`, `wizard`, `remux` e `recover` (`fluxcapture-cli help`).
Binários prontos: `cargo xtask dist` gera `dist/fluxcapture.exe`, `dist/fluxcapture-cli.exe` e o
instalador `dist/FluxCapture-Setup-<versão>.exe` (sem DLLs extras; o Windows já traz Media
Foundation, Direct3D 11 e WASAPI).

**Modo portátil**: crie um arquivo `portable.txt` ao lado do `.exe`; a configuração fica em `config\`
nessa pasta. `FLUXCAPTURE_CONFIG_DIR` também escolhe a pasta de configuração.

## Arquitetura

```
time (L0) ─ mux, audio, telemetry (L1) ─ platform (L2, único com unsafe) ─ settings, pipeline (L3)
          ─ engine (L4) ─ ui (L5) ─ apps: fluxcapture, fluxcapture-cli, fluxcapture-setup
```

Detalhes em [docs/architecture.md](docs/architecture.md) e nas decisões em [docs/decisions](docs/decisions).
Regras de contribuição (nunca travar, nunca perder uma gravação, medir antes de afirmar) em
[AGENTS.md](AGENTS.md). Antes de cada commit: `cargo xtask ci`.

## Licenças

O produto não usa FFmpeg nem código GPL: os codecs são os do Windows e os contêineres (Matroska, MP4)
foram implementados a partir das especificações públicas. ffmpeg/ffprobe aparecem apenas como
oráculos nos testes. Licença declarada no workspace: MIT OR Apache-2.0.
