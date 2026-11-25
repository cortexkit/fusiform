# Models.dev v2 cost units

`CostUnitV2Values` in models.dev `schema-v2.ts` at `397db8d`, lines 481–489,
defines the quantity basis. Amounts are in USD.

| Unit | Published quantity basis | Served key and denominator |
| --- | --- | --- |
| `token` | 1,000,000 tokens | Admitted base `rate.<role>`; 1,000,000 tokens implicit. Audio token roles additionally require `ChargeUnitEvidence`. |
| `character` | 1,000,000 characters | Text input only: `rate.input.per_character`, `per: 1000000`. Other roles are unserved. |
| `image` | 1 image | Unserved; an image is not a request. |
| `megapixel` | 1 megapixel | Unserved; there is no admitted megapixel key. |
| `second` | 1 second | Audio input only: `rate.input_audio.per_second`, `per: 1`. Other roles are unserved. |
| `minute` | 1 minute | Audio input only: `rate.input_audio.per_second`, `per: 60`. Other roles are unserved. |

The published amount is unchanged when the denominator is explicit. None of
these units prices a request. A value outside the list, such as `hour`, has no
basis in this schema and is unserved.
