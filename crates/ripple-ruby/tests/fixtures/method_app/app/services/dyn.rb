class Dyn
  def call_it(record, name)
    Order.find(1)
    record.public_send(name)
  end

  def quiet_method
    Order.count
  end

  def self_dispatch
    send(params_key)
  end

  def params_key
    :quiet_method
  end
end
