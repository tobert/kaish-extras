## Totals (cells = one first command per run x task)

336 cells from 12 runs x 28 tasks; 121 distinct command lines.

| bin | cells | cold (6 runs) | warm (6 runs) | distinct spellings |
|---|---|---|---|---|
| WORKS | 222 | 93 | 129 | 74 |
| USELESS-ERROR | 48 | 27 | 21 | 20 |
| SILENT-WRONG | 15 | 6 | 9 | 7 |
| USEFUL-REFUSAL | 0 | 0 | 0 | 0 |
| NOT-ALLOWED-BY-DESIGN | 51 | 42 | 9 | 20 |
| total | 336 | 168 | 168 | 121 |

Cosmetic divergences inside WORKS cells (tag: cells): C-cl0: 28, C-enc: 1, C-hdr: 29, C-ua: 77, C-wrote: 18, kaish: 3.

### Per run

| run | WORKS | USELESS-ERROR | SILENT-WRONG | USEFUL-REFUSAL | NOT-ALLOWED-BY-DESIGN |
|---|---|---|---|---|---|
| claude-haiku-5.5 cold1 | 15 | 5 | 1 | 0 | 7 |
| claude-haiku-5.5 warm1 | 22 | 3 | 1 | 0 | 2 |
| deepseek-flash cold1 | 16 | 4 | 1 | 0 | 7 |
| deepseek-flash cold2 | 16 | 4 | 1 | 0 | 7 |
| deepseek-flash warm1 | 22 | 3 | 3 | 0 | 0 |
| deepseek-flash warm2 | 23 | 4 | 1 | 0 | 0 |
| gemini-3.5-flash cold1 | 15 | 5 | 1 | 0 | 7 |
| gemini-3.5-flash warm1 | 16 | 4 | 1 | 0 | 7 |
| glm-5.3-flash cold1 | 16 | 4 | 1 | 0 | 7 |
| glm-5.3-flash warm1 | 23 | 3 | 2 | 0 | 0 |
| qwen3.8-flash cold1 | 15 | 5 | 1 | 0 | 7 |
| qwen3.8-flash warm1 | 23 | 4 | 1 | 0 | 0 |

## Spellings per task (runs that typed it -> bin; tags in brackets)

### T1. GET http://127.0.0.1:8765/json and print the response body.

- 10 (cold 4, warm 6) **WORKS**: `curl http://127.0.0.1:8765/json`
- 2 (cold 2, warm 0) **WORKS**: `curl -s http://127.0.0.1:8765/json`

### T2. GET http://127.0.0.1:8765/text with no progress output, but still show an error message if the request fails.

- 6 (cold 5, warm 1) **WORKS**: `curl -sS http://127.0.0.1:8765/text`
- 3 (cold 0, warm 3) **WORKS**: `curl http://127.0.0.1:8765/text`
- 1 (cold 0, warm 1) **WORKS**: `curl http://127.0.0.1:8765/text 2>/tmp/kaish-curl.err \|\| cat /tmp/kaish-curl.err >&2`
- 1 (cold 1, warm 0) **WORKS**: `curl -s -S http://127.0.0.1:8765/text`
- 1 (cold 0, warm 1) **WORKS**: `curl --fail http://127.0.0.1:8765/text \|\| echo 'request failed' >&2`

### T3. Show only the response headers of http://127.0.0.1:8765/json (no body).

- 11 (cold 5, warm 6) **WORKS** [C-hdr]: `curl -I http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **WORKS** [C-hdr]: `curl -sI http://127.0.0.1:8765/json`

### T4. Print the response headers followed by the body of http://127.0.0.1:8765/json.

- 11 (cold 5, warm 6) **WORKS** [C-hdr]: `curl -i http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **WORKS** [C-hdr]: `curl -si http://127.0.0.1:8765/json`

### T5. http://127.0.0.1:8765/redirect redirects to /json. Print the final body, following the redirect.

- 11 (cold 5, warm 6) **WORKS**: `curl -L http://127.0.0.1:8765/redirect`
- 1 (cold 1, warm 0) **WORKS**: `curl -sL http://127.0.0.1:8765/redirect`

### T6. Request http://127.0.0.1:8765/status/404 so that the command exits non-zero because of the HTTP error status (do not rely on parsing the output).

- 10 (cold 4, warm 6) **WORKS**: `curl -f http://127.0.0.1:8765/status/404`
- 1 (cold 1, warm 0) **WORKS**: `curl --fail http://127.0.0.1:8765/status/404`
- 1 (cold 1, warm 0) **WORKS**: `curl -sf http://127.0.0.1:8765/status/404`

### T7. Request http://127.0.0.1:8765/status/404, exit non-zero on the HTTP error, but still print the error response body.

- 6 (cold 5, warm 1) **USELESS-ERROR**: `curl --fail-with-body http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl http://127.0.0.1:8765/status/404; curl -f http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -f http://127.0.0.1:8765/status/404 2>/dev/null \|\| { curl http://127.0.0.1:8765/status/404 2>/dev/null; false; }`
- 1 (cold 0, warm 1) **WORKS**: `curl -f http://127.0.0.1:8765/status/404 \|\| { curl http://127.0.0.1:8765/status/404; false; }`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -sS --fail-with-body http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS** [C-hdr]: `curl -i http://127.0.0.1:8765/status/404; curl -f http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -i http://127.0.0.1:8765/status/404 \| awk 'NR==1{code=$2} /^[[:space:]]*$/{body=1; next} body{print} END{exit(code>=400)}'`

### T8. Download http://127.0.0.1:8765/file/report.txt and save it in the current directory under the name the URL gives it (report.txt).

- 6 (cold 5, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -O http://127.0.0.1:8765/file/report.txt`
- 5 (cold 0, warm 5) **WORKS** [C-wrote]: `curl -o report.txt http://127.0.0.1:8765/file/report.txt`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -sO http://127.0.0.1:8765/file/report.txt`

### T9. Save the body of http://127.0.0.1:8765/json to a local file named out.json.

- 11 (cold 5, warm 6) **WORKS** [C-wrote]: `curl -o out.json http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **WORKS** [C-wrote]: `curl -s -o out.json http://127.0.0.1:8765/json`

### T10. POST the JSON object {"name":"widget"} to http://127.0.0.1:8765/echo with the right JSON content type.

- 8 (cold 2, warm 6) **WORKS** [C-ua]: `curl -H 'Content-Type: application/json' -d '{"name":"widget"}' http://127.0.0.1:8765/echo`
- 2 (cold 2, warm 0) **WORKS** [C-ua]: `curl -X POST -H 'Content-Type: application/json' -d '{"name":"widget"}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -H "Content-Type: application/json" -d '{"name":"widget"}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -H 'Content-Type: application/json' -d '{"name":"widget"}' http://127.0.0.1:8765/echo`

### T11. POST the contents of the local file payload.json to http://127.0.0.1:8765/echo as JSON.

- 6 (cold 2, warm 4) **USELESS-ERROR**: `curl -H 'Content-Type: application/json' --data-binary @payload.json http://127.0.0.1:8765/echo`
- 3 (cold 1, warm 2) **USELESS-ERROR**: `curl -H 'Content-Type: application/json' -d @payload.json http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -X POST -H 'Content-Type: application/json' --data-binary @payload.json http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -H "Content-Type: application/json" -d @payload.json http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -s -H 'Content-Type: application/json' -d @payload.json http://127.0.0.1:8765/echo`

### T12. POST the form fields name=widget and qty=3 to http://127.0.0.1:8765/echo as an ordinary HTML form submission.

- 4 (cold 1, warm 3) **WORKS** [C-ua]: `curl -d name=widget -d qty=3 http://127.0.0.1:8765/echo`
- 4 (cold 1, warm 3) **WORKS** [C-ua]: `curl -d 'name=widget' -d 'qty=3' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -X POST -d 'name=widget&qty=3' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -d "name=widget&qty=3" http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -d name=widget -d qty=3 http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -d 'name=widget&qty=3' http://127.0.0.1:8765/echo`

### T13. Send a PUT request with the JSON body {"qty":5} to http://127.0.0.1:8765/echo.

- 10 (cold 4, warm 6) **WORKS** [C-ua]: `curl -X PUT -H 'Content-Type: application/json' -d '{"qty":5}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -X PUT -H "Content-Type: application/json" -d '{"qty":5}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -X PUT -H 'Content-Type: application/json' -d '{"qty":5}' http://127.0.0.1:8765/echo`

### T14. Send a DELETE request to http://127.0.0.1:8765/echo.

- 11 (cold 5, warm 6) **WORKS** [C-ua C-cl0]: `curl -X DELETE http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua C-cl0]: `curl -s -X DELETE http://127.0.0.1:8765/echo`

### T15. Send a PATCH request with the JSON body {"qty":6} to http://127.0.0.1:8765/echo.

- 10 (cold 4, warm 6) **WORKS** [C-ua]: `curl -X PATCH -H 'Content-Type: application/json' -d '{"qty":6}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -X PATCH -H "Content-Type: application/json" -d '{"qty":6}' http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua]: `curl -s -X PATCH -H 'Content-Type: application/json' -d '{"qty":6}' http://127.0.0.1:8765/echo`

### T16. GET http://127.0.0.1:8765/bearer with the bearer token abc123 in the Authorization header.

- 10 (cold 4, warm 6) **WORKS**: `curl -H 'Authorization: Bearer abc123' http://127.0.0.1:8765/bearer`
- 1 (cold 1, warm 0) **WORKS**: `curl -H "Authorization: Bearer abc123" http://127.0.0.1:8765/bearer`
- 1 (cold 1, warm 0) **WORKS**: `curl -s -H 'Authorization: Bearer abc123' http://127.0.0.1:8765/bearer`

### T17. GET http://127.0.0.1:8765/basic-auth using HTTP basic auth with user alice and password s3cret.

- 10 (cold 5, warm 5) **WORKS**: `curl -u alice:s3cret http://127.0.0.1:8765/basic-auth`
- 1 (cold 1, warm 0) **WORKS**: `curl -s -u alice:s3cret http://127.0.0.1:8765/basic-auth`
- 1 (cold 0, warm 1) **WORKS**: `curl -u 'alice:s3cret' http://127.0.0.1:8765/basic-auth`

### T18. GET http://127.0.0.1:8765/echo with the User-Agent set to survey-bot/1.0.

- 5 (cold 3, warm 2) **WORKS** [C-ua C-cl0]: `curl -A 'survey-bot/1.0' http://127.0.0.1:8765/echo`
- 4 (cold 1, warm 3) **WORKS** [C-ua C-cl0]: `curl -A survey-bot/1.0 http://127.0.0.1:8765/echo`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-cl0]: `curl --user-agent survey-bot/1.0 http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua C-cl0]: `curl -A "survey-bot/1.0" http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **WORKS** [C-ua C-cl0]: `curl -s -A survey-bot/1.0 http://127.0.0.1:8765/echo`

### T19. Print only the HTTP status code (a number such as 200 or 404) of http://127.0.0.1:8765/status/404, nothing else.

- 4 (cold 3, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8765/status/404`
- 2 (cold 2, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -I http://127.0.0.1:8765/status/404 \| head -n 1 \| cut -d ' ' -f 2`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `curl -i http://127.0.0.1:8765/status/404 2>/dev/null \| jq -R -r -s 'split("\n")[0] \| split(" ")[1]'`
- 1 (cold 0, warm 1) **WORKS**: `curl -I http://127.0.0.1:8765/status/404 \| jq -Rrs 'split("\n")[0] \| split(" ")[1] \| gsub("\r";"")'`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8765/status/404`
- 1 (cold 0, warm 1) **WORKS**: `curl -i http://127.0.0.1:8765/status/404 \| head -n 1 \| awk '{print $2}'`
- 1 (cold 0, warm 1) **WORKS**: `curl -I http://127.0.0.1:8765/status/404 \| awk 'NR==1{print $2}'`

### T20. Print how many seconds the request to http://127.0.0.1:8765/json took.

- 4 (cold 4, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{time_total}\n' http://127.0.0.1:8765/json`
- 2 (cold 1, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w '%{time_total}' http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **USELESS-ERROR** [kaish]: `time curl -o /dev/null http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `s=$(jq -n now); curl -o /tmp/kaish-time-body http://127.0.0.1:8765/json 2>/dev/null; e=$(jq -n now); jq -n --argjson s "$s" --argjson e "$e" '$e - $s'`
- 1 (cold 0, warm 1) **USELESS-ERROR** [kaish]: `TIMEFORMAT='%R'; time curl http://127.0.0.1:8765/json >/dev/null`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -o /dev/null -w "%{time_total}\n" http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `S=$(date +%s%N); curl -o /dev/null http://127.0.0.1:8765/json; E=$(date +%s%N); awk -v s="$S" -v e="$E" 'BEGIN { printf "%.3f\n", (e-s)/1000000000 }'`
- 1 (cold 0, warm 1) **USELESS-ERROR** [kaish]: `TIMEFORMAT='%R'; { time curl -o /dev/null http://127.0.0.1:8765/json; } 2>&1`

### T21. GET http://127.0.0.1:8765/slow (it takes 3 seconds) but give up after 1 second.

- 8 (cold 2, warm 6) **WORKS**: `curl --max-time 1 http://127.0.0.1:8765/slow`
- 3 (cold 3, warm 0) **USELESS-ERROR**: `curl -m 1 http://127.0.0.1:8765/slow`
- 1 (cold 1, warm 0) **WORKS**: `curl -s --max-time 1 http://127.0.0.1:8765/slow`

### T22. GET http://127.0.0.1:8765/flaky, which fails with 503 a couple of times; retry up to 3 times until it succeeds.

- 6 (cold 5, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl --retry 3 http://127.0.0.1:8765/flaky`
- 2 (cold 0, warm 2) **WORKS**: `for i in 1 2 3 4; do curl -f http://127.0.0.1:8765/flaky && break; done`
- 1 (cold 0, warm 1) **WORKS**: `for i in 1 2 3; do curl -f http://127.0.0.1:8765/flaky && break; done`
- 1 (cold 0, warm 1) **WORKS**: `for i in 1 2 3 4; do curl -f http://127.0.0.1:8765/flaky 2>/dev/null && break; done`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -sS --retry 3 http://127.0.0.1:8765/flaky`
- 1 (cold 0, warm 1) **WORKS**: `for i in 0 1 2 3; do curl --fail http://127.0.0.1:8765/flaky && break; done`

### T23. Upload the local file upload.txt as a multipart form file field named "file" to http://127.0.0.1:8765/upload.

- 3 (cold 1, warm 2) **NOT-ALLOWED-BY-DESIGN**: `curl -F file=@upload.txt http://127.0.0.1:8765/upload`
- 3 (cold 3, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -F 'file=@upload.txt' http://127.0.0.1:8765/upload`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf '%b' '------kaishboundary\r\nContent-Disposition: form-data; name=file; filename=upload.txt\r\nContent-Type: text/plain\r\n\r\n'; cat uplo...`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf '%s\r\n' '--BOUNDARY' 'Content-Disposition: form-data; name="file"; filename="upload.txt"' 'Content-Type: text/plain' ''; cat upload.txt; ...`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -F "file=@upload.txt" http://127.0.0.1:8765/upload`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s -F file=@upload.txt http://127.0.0.1:8765/upload`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf -- '--kaishbnd\r\nContent-Disposition: form-data; name="file"; filename="upload.txt"\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt;...`
- 1 (cold 0, warm 1) **USELESS-ERROR**: `{ printf -- '--X\r\nContent-Disposition: form-data; name="file"; filename="upload.txt"\r\nContent-Type: text/plain\r\n\r\n'; cat upload.txt; printf...`

### T24. Upload the local file upload.txt to http://127.0.0.1:8765/upload using an HTTP PUT with the file as the raw body.

- 7 (cold 1, warm 6) **USELESS-ERROR**: `curl -X PUT --data-binary @upload.txt http://127.0.0.1:8765/upload`
- 4 (cold 4, warm 0) **USELESS-ERROR**: `curl -T upload.txt http://127.0.0.1:8765/upload`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -s -T upload.txt http://127.0.0.1:8765/upload`

### T25. GET http://127.0.0.1:8765/echo with the query parameters q="hello world" and n=5 added to the URL, letting curl do the URL-encoding.

- 4 (cold 4, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -G --data-urlencode 'q=hello world' --data-urlencode 'n=5' http://127.0.0.1:8765/echo`
- 2 (cold 0, warm 2) **WORKS** [C-ua C-cl0]: `curl "http://127.0.0.1:8765/echo?q=$(jq -rn --arg v 'hello world' '$v\|@uri')&n=5"`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-cl0]: `curl 'http://127.0.0.1:8765/echo?q=hello%20world&n=5'`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -G --data-urlencode "q=hello world" --data-urlencode "n=5" http://127.0.0.1:8765/echo`
- 1 (cold 0, warm 1) **NOT-ALLOWED-BY-DESIGN**: `curl -G --data-urlencode 'q=hello world' -d n=5 http://127.0.0.1:8765/echo`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -s --get --data-urlencode 'q=hello world' --data-urlencode n=5 http://127.0.0.1:8765/echo`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-cl0]: `curl "http://127.0.0.1:8765/echo?q=$(printf 'hello world' \| jq -Rr '@uri')&n=5"`
- 1 (cold 0, warm 1) **WORKS** [C-ua C-enc]: `curl --request GET --data-urlencode 'q=hello world' --data-urlencode 'n=5' http://127.0.0.1:8765/echo`

### T26. Fetch http://127.0.0.1:8765/json and pipe it to jq to print only the "name" field's value, without quotes.

- 4 (cold 4, warm 0) **SILENT-WRONG**: `curl -s http://127.0.0.1:8765/json \| jq -r '.name'`
- 4 (cold 0, warm 4) **SILENT-WRONG**: `curl http://127.0.0.1:8765/json \| jq -r .name`
- 3 (cold 2, warm 1) **SILENT-WRONG**: `curl -s http://127.0.0.1:8765/json \| jq -r .name`
- 1 (cold 0, warm 1) **SILENT-WRONG**: `curl http://127.0.0.1:8765/json 2>/dev/null \| jq -r .name`

### T27. Fetch http://127.0.0.1:8765/json with verbose debugging output that shows the request and response headers.

- 7 (cold 5, warm 2) **NOT-ALLOWED-BY-DESIGN**: `curl -v http://127.0.0.1:8765/json`
- 2 (cold 0, warm 2) **WORKS** [C-hdr]: `curl -i http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **WORKS** [C-hdr]: `printf '%b' '> GET /json HTTP/1.1\r\n> Host: 127.0.0.1:8765\r\n> User-Agent: kaish-curl\r\n> Accept: */*\r\n>\r\n'; curl -i http://127.0.0.1:8765/json`
- 1 (cold 0, warm 1) **WORKS** [C-hdr]: `printf '> GET /json HTTP/1.1\r\n> Host: 127.0.0.1:8765\r\n> User-Agent: kaish-curl\r\n> Accept: */*\r\n>\r\n'; curl -i http://127.0.0.1:8765/json`
- 1 (cold 1, warm 0) **NOT-ALLOWED-BY-DESIGN**: `curl -sv http://127.0.0.1:8765/json 2>&1`

### T28. partial.txt is the first part of http://127.0.0.1:8765/file/report.txt. Resume the download so partial.txt becomes the complete file.

- 6 (cold 5, warm 1) **USELESS-ERROR**: `curl -C - -o partial.txt http://127.0.0.1:8765/file/report.txt`
- 1 (cold 0, warm 1) **WORKS**: `curl -H "Range: bytes=$(wc -c < partial.txt)-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
- 1 (cold 0, warm 1) **WORKS**: `curl -H "Range: bytes=$(jq -R -s 'utf8bytelength' partial.txt)-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
- 1 (cold 0, warm 1) **WORKS**: `curl -H "Range: bytes=$(( $(wc -c < partial.txt) ))-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
- 1 (cold 1, warm 0) **USELESS-ERROR**: `curl -s -C - -o partial.txt http://127.0.0.1:8765/file/report.txt`
- 1 (cold 0, warm 1) **WORKS** [C-wrote]: `curl -H "Range: bytes=$(wc -c < partial.txt)-" -o /tmp/rest http://127.0.0.1:8765/file/report.txt && cat /tmp/rest >> partial.txt && rm /tmp/rest`
- 1 (cold 0, warm 1) **WORKS**: `size=$(wc -c < partial.txt \| awk '{print $1}'); curl --fail -H "Range: bytes=$size-" http://127.0.0.1:8765/file/report.txt >> partial.txt`
