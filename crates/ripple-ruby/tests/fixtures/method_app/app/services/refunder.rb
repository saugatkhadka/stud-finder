class Refunder
  def initialize(order)
    @order = order
  end

  def run_refund
    @order.total_amount
  end

  def lookup_order
    Order.where(id: 1)
  end

  def charge_price
    Order.first.price_with_currency
  end
end
