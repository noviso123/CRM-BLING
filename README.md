# CRM Bling · WhatsApp

Painel de atendimento escrito em Rust. O servidor Axum renderiza toda a interface web com Maud; não há JavaScript no navegador. As conversas ficam em SQLite e a integração usa a API oficial WhatsApp Cloud da Meta.

## O que já funciona

- Login administrativo com cookie assinado, `HttpOnly`, `SameSite=Strict` e validade de oito horas.
- Caixa de entrada por telefone, envio de texto e modelos aprovados pela Meta, mensagens recebidas e estados de entrega recebidos por webhook.
- Contatos com nome editável; nomes de perfil incluídos nos webhooks são salvos automaticamente e podem ser corrigidos no painel.
- Busca de conversas por telefone, nome do contato ou conteúdo das mensagens, sem JavaScript no navegador.
- Verificação do webhook `GET` da Meta e validação HMAC-SHA256 do cabeçalho `X-Hub-Signature-256` em cada `POST`.
- Persistência local idempotente em SQLite; eventos repetidos da Meta não duplicam mensagens.
- Interface responsiva em português, HTML renderizado no servidor e sem dependências externas para fontes ou scripts.
- Imagem Docker não privilegiada, com banco persistido em volume.

O painel recebe texto, legendas e avisos de tipos comuns de mídia; arquivos de mídia não são baixados. O envio de modelos exige que o nome e o idioma correspondam exatamente a um modelo aprovado na conta Meta. Anexos, múltiplos usuários e sincronização histórica ainda não estão implementados.

## Requisitos

- Rust 1.85 ou mais recente e Cargo.
- Uma conta WhatsApp Business Platform configurada no Meta for Developers para tráfego real.

## Rodar localmente

```sh
cp .env.example .env
```

Edite `.env`: troque `APP_PASSWORD` por uma senha forte e `SESSION_SECRET` por pelo menos 32 bytes aleatórios. Para gerar o segredo:

```sh
openssl rand -hex 32
```

Para somente abrir o painel local, as quatro variáveis `WHATSAPP_*` podem ficar vazias. Para conectar à Meta, configure `WHATSAPP_ACCESS_TOKEN`, `WHATSAPP_PHONE_NUMBER_ID`, `WHATSAPP_APP_SECRET` e `WHATSAPP_VERIFY_TOKEN`. Não coloque credenciais em arquivos versionados.

```sh
cargo run
```

Abra `http://127.0.0.1:3000`. O banco é criado em `data/whatsapp.sqlite`. `COOKIE_SECURE=false` é apenas para desenvolvimento em `localhost` via HTTP. Em produção, mantenha `COOKIE_SECURE=true`, termine TLS em um proxy confiável e proteja os backups do banco.

## Conectar ao WhatsApp Cloud API

1. Crie um app no Meta for Developers, adicione o produto WhatsApp e associe um número do WhatsApp Business.
2. Gere um token de acesso com a permissão `whatsapp_business_messaging` e obtenha o ID do número. Informe-os em `WHATSAPP_ACCESS_TOKEN` e `WHATSAPP_PHONE_NUMBER_ID`.
3. Defina um `WHATSAPP_VERIFY_TOKEN` aleatório escolhido por você. Cadastre a URL pública `https://SEU_HOST/webhook` e esse mesmo token na configuração de Webhooks do produto WhatsApp.
4. Informe o segredo do app Meta em `WHATSAPP_APP_SECRET`; o serviço usa-o para validar a assinatura de todos os eventos recebidos.
5. Inscreva o objeto WhatsApp Business Account no campo `messages` para receber mensagens e atualizações de status.
6. Publique o serviço atrás de HTTPS e confirme a URL pública do webhook na Meta. Um servidor local sem túnel ou endereço público não pode receber webhooks.

A chamada de envio usa `https://graph.facebook.com/{GRAPH_API_VERSION}/{WHATSAPP_PHONE_NUMBER_ID}/messages`. A versão padrão é `v23.0` e pode ser alterada em `GRAPH_API_VERSION` quando sua conta estiver habilitada para outra versão. O destinatário precisa estar em formato internacional (DDI + número, 8 a 15 dígitos). O envio de texto livre está sujeito à janela de atendimento e às regras de opt-in da Meta; fora dela, use um modelo aprovado.

No painel, expanda **Enviar modelo aprovado**, informe o nome e o idioma cadastrados na Meta e os valores de substituição do corpo como um array JSON em ordem. Exemplo: `["Ana", "pedido 123"]` para os campos `{{1}}` e `{{2}}`. O nome e o idioma são enviados ao Graph API sem alteração.

## Publicar uma demonstração gratuita

O arquivo `render.yaml` prepara um serviço Web Docker no plano gratuito do Render. Envie o repositório ao GitHub, abra [render.com/deploy](https://render.com/deploy), conecte `noviso123/CRM-BLING` e publique o Blueprint. O Render gera APP_PASSWORD e SESSION_SECRET; cadastre as credenciais WhatsApp no painel seguro do serviço.

O plano gratuito suspende o serviço após inatividade e usa disco efêmero. O painel pode demorar para responder depois de dormir, e o SQLite pode ser apagado em reinícios ou novas implantações. Essa configuração oferece um endereço HTTPS para demonstração. Para atendimento contínuo e histórico confiável, será preciso hospedagem sempre ativa e banco persistente.

## Variáveis de ambiente

| Nome | Necessária | Uso |
| --- | --- | --- |
| `APP_PASSWORD` | Sim | Senha única do painel, com pelo menos 12 caracteres. |
| `SESSION_SECRET` | Sim | Chave aleatória de pelo menos 32 bytes para assinar sessões. |
| `COOKIE_SECURE` | Não | Cookies HTTPS; padrão `true`. Use `false` apenas no desenvolvimento local HTTP. |
| `APP_BIND` | Não | Sobrescreve o endereço de escuta. Sem esse valor, o app usa `0.0.0.0:$PORT` quando `PORT` existir e `127.0.0.1:3000` em desenvolvimento local. |
| `PORT` | Não | Porta fornecida por uma plataforma de hospedagem; no container local, o padrão é `3000`. |
| `DATABASE_PATH` | Não | Arquivo SQLite; padrão `data/whatsapp.sqlite`. |
| `WHATSAPP_ACCESS_TOKEN` | Para envio | Token do Graph API, mantenha-o secreto. |
| `WHATSAPP_PHONE_NUMBER_ID` | Para envio | ID do número configurado na Meta. |
| `WHATSAPP_APP_SECRET` | Para eventos | Segredo do app usado para validar assinaturas do webhook. |
| `WHATSAPP_VERIFY_TOKEN` | Para verificar webhook | Valor escolhido por você e cadastrado na Meta. |
| `GRAPH_API_VERSION` | Não | Versão do Graph API; padrão `v23.0`. |

## Docker

Configure um arquivo `.env` seguindo `.env.example` e rode:

```sh
docker compose up --build
```

O serviço publica a porta `3000` e mantém o banco em um volume Docker nomeado. Em produção, configure TLS no proxy reverso e exponha publicamente apenas HTTPS.

## Endpoints

- `GET /` — login ou painel.
- `POST /login`, `POST /logout` — sessão do painel.
- `POST /contacts` — criação e edição do nome de um contato autenticado.
- `POST /messages` — envio de texto autenticado.
- `POST /messages/template` — envio autenticado de modelo aprovado com parâmetros de texto.
- `GET /webhook` — desafio de verificação da Meta.
- `POST /webhook` — eventos da Meta assinados.
- `GET /healthz` — verificação simples de disponibilidade.
