module Priceable
  def price_with_currency
    "#{price_cents_value} USD"
  end

  def price_cents_value
    100
  end
end
