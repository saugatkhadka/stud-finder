class Order < ApplicationRecord
  include Priceable

  def total_amount
    line_total + tax_amount
  end

  def line_total
    10
  end

  def tax_amount
    1
  end

  def refund_order
    Refunder.new(self).run_refund
  end

  def badge_label
    "badge"
  end

  def never_called_anywhere
    1
  end

  def to_s
    "order"
  end

  def status=(value)
    @status = value
  end

  def abc
    2
  end

  def shared_helper
    3
  end

  def dup_name
    1
  end

  def dup_name
    2
  end

  def one_liner; 1; end
end
